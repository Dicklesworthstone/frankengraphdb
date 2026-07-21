//! FNV-1a 64-bit table pin.
//!
//! This is a *pin*, not a cryptographic commitment: it exists so CI fails
//! with an exact row-level diff when the twenty-ID invariant table changes
//! (Appendix F: "CI verifies the twenty-ID set and table hash"). Content
//! authentication of durable objects is BLAKE3 in the engine proper; the
//! registry pin only needs stability and diffability.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Canonical twenty-ID table transcript: each ID followed by `\n`, in
/// registry order.
pub fn id_table_hash(ids: &[String]) -> String {
    let mut transcript = Vec::new();
    for id in ids {
        transcript.extend_from_slice(id.as_bytes());
        transcript.push(b'\n');
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

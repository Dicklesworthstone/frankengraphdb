//! Real Chronicle crypto/FEC fixture with canonical RFC object transmission information.
use fgdb_chronicle::identity::{
    CipherDescriptor, EncodedObject, EncodingDescriptor, IdentifiedObject,
};
use fgdb_chronicle::symbolize::{RecoveryTarget, encode_object};
use fgdb_types::DatabaseSecurityNamespaceId;

pub const KEY: [u8; 32] = [0x29; 32];
pub const DEK: [u8; 32] = [0x53; 32];
pub const HEADER: &[u8] = b"aegis-multiblock";
pub const KIND: u16 = 2;

pub fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([0x76; 32])
}

pub struct Fixture {
    pub encoding: EncodedObject,
    pub protected: Vec<u8>,
    pub plaintext: Vec<u8>,
    pub records: Vec<Vec<Vec<u8>>>,
    pub sources: Vec<usize>,
}

impl Fixture {
    pub fn new(blocks: u16, sub_blocks: u16, repairs: u32) -> Self {
        Self::with_shape(3, 4093, 256, blocks, sub_blocks, 4, repairs)
    }

    pub fn with_shape(
        salt: u8,
        length: usize,
        size: u16,
        blocks: u16,
        subs: u16,
        al: u8,
        repairs: u32,
    ) -> Self {
        let (encoding, protected, plaintext) =
            Self::object(salt, length, size, blocks, subs, al, |_| {});
        let total = protected.len().div_ceil(usize::from(size));
        let sources = (0..usize::from(blocks))
            .map(|block| {
                total / usize::from(blocks) + usize::from(block < total % usize::from(blocks))
            })
            .collect();
        let records = (0..u32::from(blocks))
            .map(|block| encode_object(&encoding, &protected, KIND, block, repairs, &DEK).unwrap())
            .collect();
        Self {
            encoding,
            protected,
            plaintext,
            records,
            sources,
        }
    }

    pub fn object(
        salt: u8,
        length: usize,
        size: u16,
        blocks: u16,
        subs: u16,
        al: u8,
        change: impl FnOnce(&mut EncodingDescriptor),
    ) -> (EncodedObject, Vec<u8>, Vec<u8>) {
        let plaintext: Vec<_> = (0..length).map(|i| (i % 251) as u8 ^ salt).collect();
        let object = IdentifiedObject::new(&KEY, namespace(), KIND, HEADER, &plaintext);
        let protected = object
            .protect(
                &DEK,
                CipherDescriptor {
                    object_kind: KIND,
                    canonical_plaintext_len: length as u64,
                    codec_profile: 1,
                    compressed_len: length as u64,
                    data_crypto_profile: 1,
                    dek_id: [9; 16],
                    object_nonce: [salt; 24],
                    object_tag_len: 16,
                },
                &plaintext,
            )
            .unwrap();
        let length = protected.protected_bytes().len() as u64;
        let mut descriptor = EncodingDescriptor {
            fec_profile: 1,
            transfer_length: length,
            oti_common: (length << 24) | u64::from(size),
            oti_scheme: (u32::from(blocks) << 24) | (u32::from(subs) << 8) | u32::from(al),
            symbol_size: size,
            source_block_count: blocks,
            symbol_auth_profile: 1,
        };
        change(&mut descriptor);
        (
            protected.encode(descriptor),
            protected.protected_bytes().to_vec(),
            plaintext,
        )
    }

    pub fn target(&self) -> RecoveryTarget<'static> {
        RecoveryTarget {
            k_oid: &KEY,
            namespace: namespace(),
            object_id: self.encoding.object_id(),
            canonical_header: HEADER,
            protected_len: self.protected.len(),
        }
    }

    pub fn all(&self) -> Vec<Vec<u8>> {
        self.records.iter().flatten().cloned().collect()
    }
}

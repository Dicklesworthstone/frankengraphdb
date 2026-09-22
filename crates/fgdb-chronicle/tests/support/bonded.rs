use fgdb_chronicle::identity::{
    CipherDescriptor, EncodedObject, EncodingDescriptor, IdentifiedObject,
};
use fgdb_chronicle::symbolize::{RecoveryTarget, encode_object, source_symbol_count};
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, VerifiedObject};
use fgdb_types::DatabaseSecurityNamespaceId;

pub const KEY: [u8; 32] = [0x39; 32];
pub const DEK: [u8; 32] = [0x63; 32];
pub const HEADER: &[u8] = b"aegis-object-header";
pub const KIND: u16 = 2;
pub const SYMBOL_SIZE: u16 = 256;

pub fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(core::array::from_fn(|i| i as u8 ^ 0x5a))
}

pub struct Fixture {
    pub encoding: EncodedObject,
    pub records: Vec<Vec<u8>>,
    pub plaintext: Vec<u8>,
    pub protected_len: usize,
    pub sources: usize,
}

impl Fixture {
    pub fn new(salt: u8) -> Self {
        Self::with_source_blocks(salt, 1)
    }

    pub fn with_source_blocks(salt: u8, source_block_count: u16) -> Self {
        let plaintext: Vec<_> = (0..4096).map(|i| (i % 251) as u8 ^ salt).collect();
        let object = IdentifiedObject::new(&KEY, namespace(), KIND, HEADER, &plaintext);
        let cipher = CipherDescriptor {
            object_kind: KIND,
            canonical_plaintext_len: plaintext.len() as u64,
            codec_profile: 1,
            compressed_len: plaintext.len() as u64,
            data_crypto_profile: 1,
            dek_id: [9; 16],
            object_nonce: [salt; 24],
            object_tag_len: 16,
        };
        let protected = object.protect(&DEK, cipher, &plaintext).unwrap();
        let protected_len = protected.protected_bytes().len();
        let encoding = protected.encode(EncodingDescriptor {
            fec_profile: 1,
            transfer_length: protected_len as u64,
            oti_common: 0x0001_0002_0003_0004,
            oti_scheme: 0x0005_0006,
            symbol_size: SYMBOL_SIZE,
            source_block_count,
            symbol_auth_profile: 1,
        });
        let records =
            encode_object(&encoding, protected.protected_bytes(), KIND, 0, 128, &DEK).unwrap();
        Self {
            encoding,
            records,
            plaintext,
            protected_len,
            sources: source_symbol_count(protected_len, SYMBOL_SIZE),
        }
    }

    pub fn target(&self) -> RecoveryTarget<'static> {
        RecoveryTarget {
            k_oid: &KEY,
            namespace: namespace(),
            object_id: self.encoding.object_id(),
            canonical_header: HEADER,
            protected_len: self.protected_len,
        }
    }

    pub fn verified(&self) -> VerifiedObject {
        let mut pull = BondedPull::new(
            &self.encoding,
            self.target(),
            &DEK,
            &[DonorId(1)],
            PullLimits::default(),
        )
        .unwrap();
        for request in pull.schedule(self.sources).unwrap() {
            pull.accept(
                request.donor,
                &self.records[request.esi as usize],
                &mut Vec::new(),
            )
            .unwrap();
        }
        pull.try_recover(&mut Vec::new()).unwrap().unwrap()
    }
}

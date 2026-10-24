use super::*;

// All five pure Ed25519 examples in RFC 8032 section 7.1. Public test seeds,
// never production material. The long example contains exactly 1023 bytes.
const VECTORS: &[(&str, &str, &str, &str)] = &[
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
    (
        "f5e5767cf153319517630f226876b86c8160cc583bc013744c6bf255f5cc0ee5",
        "278117fc144c72340f67d0f2316e8386ceffbf2b2428c9c51fef7c597f1d426e",
        "08b8b2b733424243760fe426a4b54908\
         632110a66c2f6591eabd3345e3e4eb98\
         fa6e264bf09efe12ee50f8f54e9f77b1\
         e355f6c50544e23fb1433ddf73be84d8\
         79de7c0046dc4996d9e773f4bc9efe57\
         38829adb26c81b37c93a1b270b20329d\
         658675fc6ea534e0810a4432826bf58c\
         941efb65d57a338bbd2e26640f89ffbc\
         1a858efcb8550ee3a5e1998bd177e93a\
         7363c344fe6b199ee5d02e82d522c4fe\
         ba15452f80288a821a579116ec6dad2b\
         3b310da903401aa62100ab5d1a36553e\
         06203b33890cc9b832f79ef80560ccb9\
         a39ce767967ed628c6ad573cb116dbef\
         efd75499da96bd68a8a97b928a8bbc10\
         3b6621fcde2beca1231d206be6cd9ec7\
         aff6f6c94fcd7204ed3455c68c83f4a4\
         1da4af2b74ef5c53f1d8ac70bdcb7ed1\
         85ce81bd84359d44254d95629e9855a9\
         4a7c1958d1f8ada5d0532ed8a5aa3fb2\
         d17ba70eb6248e594e1a2297acbbb39d\
         502f1a8c6eb6f1ce22b3de1a1f40cc24\
         554119a831a9aad6079cad88425de6bd\
         e1a9187ebb6092cf67bf2b13fd65f270\
         88d78b7e883c8759d2c4f5c65adb7553\
         878ad575f9fad878e80a0c9ba63bcbcc\
         2732e69485bbc9c90bfbd62481d9089b\
         eccf80cfe2df16a2cf65bd92dd597b07\
         07e0917af48bbb75fed413d238f5555a\
         7a569d80c3414a8d0859dc65a46128ba\
         b27af87a71314f318c782b23ebfe808b\
         82b0ce26401d2e22f04d83d1255dc51a\
         ddd3b75a2b1ae0784504df543af8969b\
         e3ea7082ff7fc9888c144da2af58429e\
         c96031dbcad3dad9af0dcbaaaf268cb8\
         fcffead94f3c7ca495e056a9b47acdb7\
         51fb73e666c6c655ade8297297d07ad1\
         ba5e43f1bca32301651339e22904cc8c\
         42f58c30c04aafdb038dda0847dd988d\
         cda6f3bfd15c4b4c4525004aa06eeff8\
         ca61783aacec57fb3d1f92b0fe2fd1a8\
         5f6724517b65e614ad6808d6f6ee34df\
         f7310fdc82aebfd904b01e1dc54b2927\
         094b2db68d6f903b68401adebf5a7e08\
         d78ff4ef5d63653a65040cf9bfd4aca7\
         984a74d37145986780fc0b16ac451649\
         de6188a7dbdf191f64b5fc5e2ab47b57\
         f7f7276cd419c17a3ca8e1b939ae49e4\
         88acba6b965610b5480109c8b17b80e1\
         b7b750dfc7598d5d5011fd2dcc5600a3\
         2ef5b52a1ecc820e308aa342721aac09\
         43bf6686b64b2579376504ccc493d97e\
         6aed3fb0f9cd71a43dd497f01f17c0e2\
         cb3797aa2a2f256656168e6c496afc5f\
         b93246f6b1116398a346f1a641f3b041\
         e989f7914f90cc2c7fff357876e506b5\
         0d334ba77c225bc307ba537152f3f161\
         0e4eafe595f6d9d90d11faa933a15ef1\
         369546868a7f3a45a96768d40fd9d034\
         12c091c6315cf4fde7cb68606937380d\
         b2eaaa707b4c4185c32eddcdd306705e\
         4dc1ffc872eeee475a64dfac86aba41c\
         0618983f8741c5ef68d3a101e8a3b8ca\
         c60c905c15fc910840b94c00a0b9d0",
        "0aab4c900501b3e24d7cdf4663326a3a87df5e4843b2cbdb67cbf6e460fec350aa5371b1508f9f4528ecea23c436d94b5e8fcd4f681e30a6ac00a9704a188a03",
    ),
    (
        "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
        "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
    ),
];
fn unhex(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0);
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digits = core::str::from_utf8(pair).unwrap();
            u8::from_str_radix(digits, 16).unwrap()
        })
        .collect()
}
fn fixed<const N: usize>(text: &str) -> [u8; N] {
    unhex(text).try_into().unwrap()
}

#[test]
fn all_rfc8032_pure_ed25519_vectors_derive_sign_and_verify() {
    for &(seed, public, message, signature) in VECTORS {
        let key = Ed25519SigningKey::from_seed(Secret::new(fixed(seed)));
        let public = Ed25519VerifyingKey::from_bytes(fixed(public)).unwrap();
        let message = unhex(message);
        let signature: [u8; 64] = fixed(signature);
        assert_eq!(key.verifying_key(), public);
        assert_eq!(key.sign(&message), signature);
        assert_eq!(key.sign(&message), signature);
        assert_eq!(public.verify(&message, &signature), Ok(()));
    }
    assert_eq!(unhex(VECTORS[3].2).len(), 1023);
}

#[test]
fn message_signature_and_wrong_key_tampering_refuse() {
    let key = Ed25519SigningKey::from_seed(Secret::new(fixed(VECTORS[1].0)));
    let public = key.verifying_key();
    let signature = key.sign(b"signed transcript");
    assert!(public.verify(b"different transcript", &signature).is_err());
    let wrong = Ed25519SigningKey::from_seed(Secret::new([7; 32])).verifying_key();
    assert!(wrong.verify(b"signed transcript", &signature).is_err());
    for at in 0..64 {
        let mut modified = signature;
        modified[at] ^= 1;
        assert!(
            public.verify(b"signed transcript", &modified).is_err(),
            "byte={at}"
        );
    }
    assert_eq!(format!("{key:?}"), "Ed25519SigningKey([REDACTED])");
}

#[test]
fn noncanonical_scalars_and_points_are_not_reduced_during_verification() {
    let key = Ed25519SigningKey::from_seed(Secret::new(fixed(VECTORS[0].0)));
    let public = key.verifying_key();
    let original = key.sign(b"");
    let mut plus_order = original;
    let mut carry = 0u16;
    for (byte, order) in plus_order[32..].iter_mut().zip(scalar::ORDER) {
        let sum = u16::from(*byte) + u16::from(order) + carry;
        *byte = sum as u8;
        carry = sum >> 8;
    }
    assert!(public.verify(b"", &plus_order).is_err());
    for scalar in [scalar::ORDER, [0xff; 32]] {
        let mut signature = original;
        signature[32..].copy_from_slice(&scalar);
        assert!(public.verify(b"", &signature).is_err());
    }
    let mut p = [0xff; 32];
    p[0] = 0xed;
    p[31] = 0x7f;
    let mut bad_sign = [0; 32];
    bad_sign[0] = 1;
    bad_sign[31] = 0x80;
    for point in [p, [0xff; 32], bad_sign] {
        assert!(Ed25519VerifyingKey::from_bytes(point).is_err());
        let mut signature = original;
        signature[..32].copy_from_slice(&point);
        assert!(public.verify(b"", &signature).is_err());
    }
}

#[test]
fn weak_and_mixed_order_public_keys_and_torsion_modified_r_refuse() {
    let order_two = Point {
        x: Field::ZERO,
        y: Field::ONE.negate(),
        z: Field::ONE,
        t: Field::ZERO,
    };
    for encoded in [
        Point::identity().encode(),
        order_two.encode(),
        [0; 32],
        Point::base().add(&order_two).encode(),
    ] {
        assert!(
            Point::decode(&encoded).is_some(),
            "valid curve point, wrong subgroup"
        );
        assert_eq!(
            Ed25519VerifyingKey::from_bytes(encoded),
            Err(Ed25519Error::InvalidPublicKey)
        );
    }
    let key = Ed25519SigningKey::from_seed(Secret::new([11; 32]));
    let mut signature = key.sign(b"cofactorless");
    let r = Point::decode(signature[..32].try_into().unwrap()).unwrap();
    signature[..32].copy_from_slice(&r.add(&order_two).encode());
    assert!(
        key.verifying_key()
            .verify(b"cofactorless", &signature)
            .is_err()
    );
}

#[test]
fn equation_allows_identity_r_only_with_a_valid_response_and_strong_key() {
    // Pure Ed25519 can mathematically have r=0. Rejecting weak PUBLIC KEYS
    // does not reject this valid cofactorless equation for a strong key.
    let key = Ed25519SigningKey::from_seed(Secret::new([19; 32]));
    let public = key.verifying_key();
    let r = Point::identity().encode();
    let k = challenge(&r, public.as_bytes(), b"zero nonce equation");
    let a = key.expanded.expose()[..32].try_into().unwrap();
    let s = scalar::mul_add(k.expose(), a, &[0; 32]);
    let mut signature = [0; 64];
    signature[..32].copy_from_slice(&r);
    signature[32..].copy_from_slice(s.expose());
    assert!(public.verify(b"zero nonce equation", &signature).is_ok());
}

#[test]
fn fixed_seed_roundtrips_across_hash_block_boundaries() {
    let message: Vec<_> = (0..1025).map(|i| (i % 251) as u8).collect();
    for (index, length) in [
        0, 1, 31, 32, 63, 64, 79, 80, 111, 112, 127, 128, 129, 255, 256, 1025,
    ]
    .into_iter()
    .enumerate()
    {
        let key = Ed25519SigningKey::from_seed(Secret::new([index as u8; 32]));
        let public = Ed25519VerifyingKey::from_bytes(key.verifying_key().to_bytes()).unwrap();
        let signature = key.sign(&message[..length]);
        assert_eq!(key.sign(&message[..length]), signature);
        assert_eq!(public.verify(&message[..length], &signature), Ok(()));
    }
}

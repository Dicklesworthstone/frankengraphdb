//! Public-API witnesses for the exact registered Ed25519 acceptance policy.
//!
//! These are not just bit flips of a valid signature. For the torsion-R cases,
//! recomputing the challenge and response makes [8S]B = [8]R + [8k]A TRUE
//! while [S]B = R + [k]A is FALSE. Multiplying the verification equation by
//! the cofactor would therefore turn a required refusal into an acceptance.
//!
//! Independently constructed with integer arithmetic modulo 2^255-19 and
//! SHA-512; the ordinary public key was cross-checked with OpenSSL. The public
//! seed is bytes 0..31, a is its clamped expansion, and r=97. For each R=rB+T,
//! k=SHA512(R||A||message) mod L and S=(r+k*a) mod L. T has order two or four.
//! The mixed-A case instead uses A=aB+T2, R=rB and an even challenge, so the
//! UNCOFACTORED equation holds but the key must still fail subgroup admission.
//! The identity-R response is S=k*a with the ordinary strong A.
//!
//! These test fixtures and their independently checked equations do not prove
//! timing safety or substitute for executing the Rust tests and external audit.

use fgdb_crypto::{
    Ed25519Error, Ed25519VerifyingKey, SIGNATURE_PROFILE_ED25519_STRICT, SignatureError,
    verify_signature,
};

const PUBLIC: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
const IDENTITY_R_MESSAGE: &[u8] = b"fgdb:signature-policy:v1:identity-R";
const IDENTITY_R_SIGNATURE: &str = "0100000000000000000000000000000000000000000000000000000000000000a085dea0151a5676814cbc3bbb485a202f43d1019fb179e32d14ce58bc23910e";

fn unhex<const N: usize>(text: &str) -> [u8; N] {
    assert_eq!(text.len(), 2 * N);
    let mut bytes = [0; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
    }
    bytes
}

#[test]
fn registered_profile_refuses_genuine_cofactor_only_equations() {
    let public = unhex::<32>(PUBLIC);
    let key = Ed25519VerifyingKey::from_bytes(public).unwrap();
    let cases: &[(&[u8], &str)] = &[
        (
            b"fgdb:signature-policy:v1:torsion2-R",
            "72f0f974221e2c6926a468a8612f83363543a50ed74805ccc708535b7a23e8f4d0b47059bac5c74e1e865970cb4c3aea8f21ce41b7327e86806a0005a48ecb05",
        ),
        (
            b"fgdb:signature-policy:v1:torsion4-R",
            "50e8f1e2e13c0d3750bf7b1454696768a97d4d3b65f4899aa4024165d4aace1a1c154fe561e62f7e254501a6a064f96f9b5b4e39205495e1d3edff7562c9d902",
        ),
    ];
    for &(message, signature) in cases {
        let signature = unhex::<64>(signature);
        assert_eq!(
            key.verify(message, &signature),
            Err(Ed25519Error::InvalidSignature)
        );
        assert_eq!(
            verify_signature(
                SIGNATURE_PROFILE_ED25519_STRICT,
                &public,
                message,
                &signature
            ),
            Err(SignatureError::Primitive(Ed25519Error::InvalidSignature)),
        );
    }
}

#[test]
fn valid_uncofactored_equation_does_not_admit_a_mixed_order_key() {
    let public = unhex::<32>("ea5ef8400c31ef41e28f22e718b43f66981b29cf645af2a0e223799bedaace47");
    let message = b"fgdb:signature-policy:v1:mixed-A:1";
    let signature = unhex::<64>(
        "7b0f068bdde1d396d95b97579ed07cc9cabc5af128b7fa3338f7aca485dc170b55eada46eb7413ae6fe7ca77e420530013e9d514c6fc8cfa59a4a86b1a7d8008",
    );
    // OpenSSL accepts this equation; this deliberately stricter key-admission
    // policy does not. Matching a permissive oracle is not the acceptance law.
    assert_eq!(
        Ed25519VerifyingKey::from_bytes(public),
        Err(Ed25519Error::InvalidPublicKey),
    );
    assert_eq!(
        verify_signature(
            SIGNATURE_PROFILE_ED25519_STRICT,
            &public,
            message,
            &signature
        ),
        Err(SignatureError::Primitive(Ed25519Error::InvalidPublicKey)),
    );
}

#[test]
fn a_strong_key_and_valid_response_may_have_identity_r() {
    let public = unhex::<32>(PUBLIC);
    let signature = unhex::<64>(IDENTITY_R_SIGNATURE);
    let key = Ed25519VerifyingKey::from_bytes(public).unwrap();
    assert_eq!(key.verify(IDENTITY_R_MESSAGE, &signature), Ok(()));
    assert_eq!(
        verify_signature(
            SIGNATURE_PROFILE_ED25519_STRICT,
            &public,
            IDENTITY_R_MESSAGE,
            &signature,
        ),
        Ok(()),
    );
}

#[test]
fn adding_the_group_order_to_a_valid_response_is_not_silently_reduced() {
    let public = unhex::<32>(PUBLIC);
    let mut signature = unhex::<64>(IDENTITY_R_SIGNATURE);
    let order = unhex::<32>("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
    let mut carry = 0_u16;
    for (byte, order_byte) in signature[32..].iter_mut().zip(order) {
        let sum = u16::from(*byte) + u16::from(order_byte) + carry;
        *byte = sum as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0);
    let key = Ed25519VerifyingKey::from_bytes(public).unwrap();
    assert_eq!(
        key.verify(IDENTITY_R_MESSAGE, &signature),
        Err(Ed25519Error::InvalidSignature)
    );
    assert_eq!(
        verify_signature(
            SIGNATURE_PROFILE_ED25519_STRICT,
            &public,
            IDENTITY_R_MESSAGE,
            &signature,
        ),
        Err(SignatureError::Primitive(Ed25519Error::InvalidSignature)),
    );
}

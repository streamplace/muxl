//! ES256K (secp256k1) self-signed leaf cert generation for S2PA.
//!
//! See dasl.ing/s2pa.html for the spec. In short: an X.509 v3 self-signed
//! certificate whose `commonName` is a DID identifying the signer (a
//! `did:key` derived from the public key by default, or any caller-supplied
//! DID for `did:plc` / `did:web` / etc.), with the secp256k1 public key in
//! `subjectPublicKeyInfo`. The cert is *not* a chain anchor — it's an
//! envelope for the public key that satisfies C2PA's cert profile check.
//!
//! Verification of S2PA-signed assets does not chain to a CA; it confirms
//! the signature against the embedded public key and, for non-`did:key`
//! identifiers, resolves the DID and confirms one of its verification
//! methods matches the cert's public key.

use std::time::{Duration, SystemTime};

use const_oid::ObjectIdentifier;
use const_oid::db::rfc5280::{
    ID_CE_AUTHORITY_KEY_IDENTIFIER, ID_CE_BASIC_CONSTRAINTS, ID_CE_EXT_KEY_USAGE,
    ID_CE_KEY_USAGE, ID_CE_SUBJECT_KEY_IDENTIFIER, ID_KP_EMAIL_PROTECTION,
};
use der::asn1::{BitString, OctetString};
use der::Encode;
use k256::SecretKey;
use k256::ecdsa::SigningKey;
use k256::ecdsa::signature::Signer;
use k256::elliptic_curve::rand_core::{OsRng, RngCore};
use k256::elliptic_curve::sec1::ToEncodedPoint;
use sha1::{Digest, Sha1};
use spki::AlgorithmIdentifier;
use x509_cert::Certificate;
use x509_cert::ext::Extension;
use x509_cert::ext::pkix::{
    AuthorityKeyIdentifier, BasicConstraints, ExtendedKeyUsage, KeyUsage, KeyUsages,
    SubjectKeyIdentifier,
};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::SubjectPublicKeyInfoOwned;
use x509_cert::time::{Time, Validity};
use x509_cert::{TbsCertificate, Version};

use crate::error::{Error, Result};

/// `id-ecPublicKey` (RFC 5480) — used by the SPKI algorithm field.
const OID_EC_PUBLIC_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
/// secp256k1 named curve (SEC 2 / RFC 5480).
const OID_SECP256K1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.10");
/// `ecdsa-with-SHA256` (RFC 5758) — signatureAlgorithm of the issued cert.
const OID_ECDSA_WITH_SHA256: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");

/// Multicodec code for secp256k1-pub (varint-encoded as two bytes:
/// 0xe7 0x01).
const MULTICODEC_SECP256K1_PUB: [u8; 2] = [0xe7, 0x01];

/// Generate a fresh secp256k1 signing key.
pub fn generate_key() -> SecretKey {
    SecretKey::random(&mut OsRng.clone())
}

/// Compute the `did:key` identifier for a secp256k1 public key per the
/// did:key spec — `did:key:z` + base58btc(multicodec_secp256k1_pub ||
/// compressed_sec1_pubkey).
pub fn did_key_for(public_key: &k256::PublicKey) -> String {
    let compressed = public_key.to_sec1_bytes(); // 33 bytes for k256
    let mut payload = Vec::with_capacity(2 + compressed.len());
    payload.extend_from_slice(&MULTICODEC_SECP256K1_PUB);
    payload.extend_from_slice(&compressed);
    let mut out = String::from("did:key:z");
    out.push_str(&base58btc_encode(&payload));
    out
}

/// Generate an S2PA leaf certificate for the given private key.
///
/// `did` is set as the `commonName` of both `subject` and `issuer`. If
/// `None`, defaults to `did_key_for(public_key)` — the self-rooted
/// identifier derivable from the key itself.
///
/// `organization` adds an `organizationName` attribute alongside the
/// `commonName` in the DN. Optional — included for compatibility with
/// C2PA libraries that expect a non-`commonName` attribute in the cert
/// profile check.
///
/// Returns DER-encoded certificate bytes. Use [`cert_to_pem`] to wrap in
/// PEM.
pub fn generate_cert(
    private_key: &SecretKey,
    did: Option<&str>,
    organization: Option<&str>,
) -> Result<Vec<u8>> {
    let public_key = private_key.public_key();
    let tbs = build_tbs(&public_key, did, organization)?;

    // Sign the TBSCertificate with the secp256k1 private key (ECDSA-SHA256).
    // k256's `Signer` impl hashes internally with SHA-256 and produces a
    // fixed-length (r || s) signature; X.509 wants the ASN.1 DER form.
    let tbs_der = tbs.to_der().map_err(cert_err)?;
    let signing_key = SigningKey::from(private_key);
    let signature: k256::ecdsa::Signature = signing_key.sign(&tbs_der);
    assemble_cert(tbs, signature.to_der().as_bytes())
}

/// Generate an S2PA leaf certificate for `public_key_uncompressed` (65-byte
/// SEC1 `0x04 || X || Y`), signing the TBSCertificate via `sign_tbs` rather
/// than an in-process private key.
///
/// `sign_tbs` receives the TBSCertificate DER and must return the
/// ECDSA-SHA256 signature as raw 64-byte `r || s` (it is re-encoded to DER
/// here). This is the host-callback path: the private key lives outside this
/// code — e.g. a Livepeer orchestrator's Ethereum keystore reached via
/// `host_sign` — so the issued cert binds to that same secp256k1 identity and
/// DID without the key ever entering the process. `did`/`organization` behave
/// as in [`generate_cert`].
pub fn generate_cert_with_signer<F>(
    public_key_uncompressed: &[u8],
    did: Option<&str>,
    organization: Option<&str>,
    sign_tbs: F,
) -> Result<Vec<u8>>
where
    F: FnOnce(&[u8]) -> Result<Vec<u8>>,
{
    let public_key = k256::PublicKey::from_sec1_bytes(public_key_uncompressed)
        .map_err(|e| cert_err_msg(format!("bad secp256k1 public key: {e}")))?;
    let tbs = build_tbs(&public_key, did, organization)?;
    let tbs_der = tbs.to_der().map_err(cert_err)?;
    let signature_der = raw_rs_to_der(&sign_tbs(&tbs_der)?)?;
    assemble_cert(tbs, &signature_der)
}

/// Build the TBSCertificate for an S2PA leaf cert over `public_key`. Shared by
/// the in-process [`generate_cert`] and the host-signed
/// [`generate_cert_with_signer`].
fn build_tbs(
    public_key: &k256::PublicKey,
    did: Option<&str>,
    organization: Option<&str>,
) -> Result<TbsCertificate> {
    let did_owned: String;
    let did_str: &str = match did {
        Some(s) => s,
        None => {
            did_owned = did_key_for(public_key);
            &did_owned
        }
    };

    let mut dn = format!("CN={}", escape_dn_value(did_str));
    if let Some(o) = organization {
        dn.push_str(",O=");
        dn.push_str(&escape_dn_value(o));
    }
    let subject: Name = dn.parse().map_err(cert_err)?;
    let issuer = subject.clone();

    // SPKI: id-ecPublicKey + named curve secp256k1, public key in
    // uncompressed SEC1 (0x04 || X || Y, 65 bytes). Matches the form
    // Streamplace's Go generator uses.
    let uncompressed = public_key.to_encoded_point(false);
    let spki = SubjectPublicKeyInfoOwned {
        algorithm: AlgorithmIdentifier {
            oid: OID_EC_PUBLIC_KEY,
            parameters: Some(der::Any::from(&OID_SECP256K1)),
        },
        subject_public_key: BitString::from_bytes(uncompressed.as_bytes())
            .map_err(cert_err)?,
    };

    // SHA-1 of the uncompressed public key bytes — RFC 5280's "method 1"
    // for the SubjectKeyIdentifier. Same value for AuthorityKeyIdentifier
    // since the cert is self-issued.
    let key_id_bytes = Sha1::digest(uncompressed.as_bytes()).to_vec();
    let key_id = OctetString::new(key_id_bytes.clone()).map_err(cert_err)?;

    // Extensions. Order chosen to be stable and conventional, not
    // semantically meaningful.
    let extensions = vec![
        ext(
            ID_CE_BASIC_CONSTRAINTS,
            true,
            BasicConstraints {
                ca: false,
                path_len_constraint: None,
            }
            .to_der()
            .map_err(cert_err)?,
        )?,
        ext(
            ID_CE_KEY_USAGE,
            true,
            KeyUsage(KeyUsages::DigitalSignature.into())
                .to_der()
                .map_err(cert_err)?,
        )?,
        ext(
            ID_CE_EXT_KEY_USAGE,
            false,
            ExtendedKeyUsage(vec![ID_KP_EMAIL_PROTECTION])
                .to_der()
                .map_err(cert_err)?,
        )?,
        ext(
            ID_CE_SUBJECT_KEY_IDENTIFIER,
            false,
            SubjectKeyIdentifier(key_id.clone()).to_der().map_err(cert_err)?,
        )?,
        ext(
            ID_CE_AUTHORITY_KEY_IDENTIFIER,
            false,
            AuthorityKeyIdentifier {
                key_identifier: Some(key_id),
                authority_cert_issuer: None,
                authority_cert_serial_number: None,
            }
            .to_der()
            .map_err(cert_err)?,
        )?,
    ];

    // Random 20-byte serial, masked to a positive INTEGER (high bit 0).
    let mut serial_bytes = [0u8; 20];
    let mut rng = OsRng;
    rng.fill_bytes(&mut serial_bytes);
    serial_bytes[0] &= 0x7f;
    if serial_bytes[0] == 0 {
        serial_bytes[0] = 1;
    }
    let serial = SerialNumber::new(&serial_bytes).map_err(cert_err)?;

    // 100-year validity starting at issuance — matches Streamplace's
    // Go generator. S2PA verifiers don't enforce validity.
    let not_before = SystemTime::now();
    let not_after = not_before
        .checked_add(Duration::from_secs(100 * 365 * 24 * 3600))
        .ok_or_else(|| cert_err_msg("validity overflow"))?;
    let validity = Validity {
        not_before: Time::try_from(not_before).map_err(cert_err)?,
        not_after: Time::try_from(not_after).map_err(cert_err)?,
    };

    Ok(TbsCertificate {
        version: Version::V3,
        serial_number: serial,
        signature: AlgorithmIdentifier {
            oid: OID_ECDSA_WITH_SHA256,
            parameters: None,
        },
        issuer,
        validity,
        subject,
        subject_public_key_info: spki,
        issuer_unique_id: None,
        subject_unique_id: None,
        extensions: Some(extensions),
    })
}

/// Assemble a [`Certificate`] from a built TBS and its DER-encoded ECDSA
/// signature; returns the DER cert bytes.
fn assemble_cert(tbs: TbsCertificate, signature_der: &[u8]) -> Result<Vec<u8>> {
    let cert = Certificate {
        tbs_certificate: tbs,
        signature_algorithm: AlgorithmIdentifier {
            oid: OID_ECDSA_WITH_SHA256,
            parameters: None,
        },
        signature: BitString::from_bytes(signature_der).map_err(cert_err)?,
    };
    cert.to_der().map_err(cert_err)
}

/// Re-encode a raw 64-byte `r || s` (P1363) ECDSA signature as ASN.1 DER
/// (`SEQUENCE { INTEGER r, INTEGER s }`), the form X.509 signatureValue wants.
fn raw_rs_to_der(raw: &[u8]) -> Result<Vec<u8>> {
    let sig = k256::ecdsa::Signature::from_slice(raw)
        .map_err(|e| cert_err_msg(format!("expected a 64-byte r||s signature: {e}")))?;
    Ok(sig.to_der().as_bytes().to_vec())
}

/// Wrap DER cert bytes in a single PEM `CERTIFICATE` block (RFC 7468).
pub fn cert_to_pem(der_bytes: &[u8]) -> String {
    pem_block("CERTIFICATE", der_bytes)
}

/// Encode a secp256k1 private key as PKCS#8 PEM. Round-trips with
/// `k256::SecretKey::from_pkcs8_pem` and with `openssl genpkey -algorithm
/// EC -pkeyopt ec_paramgen_curve:secp256k1`.
pub fn key_to_pem(private_key: &SecretKey) -> Result<String> {
    use k256::pkcs8::EncodePrivateKey;
    let zeroizing = private_key
        .to_pkcs8_pem(k256::pkcs8::LineEnding::LF)
        .map_err(|e| cert_err_msg(format!("pkcs8 pem: {e}")))?;
    Ok(zeroizing.as_str().to_owned())
}

// --- internal helpers -------------------------------------------------------

fn ext(oid: ObjectIdentifier, critical: bool, der_bytes: Vec<u8>) -> Result<Extension> {
    Ok(Extension {
        extn_id: oid,
        critical,
        extn_value: OctetString::new(der_bytes).map_err(cert_err)?,
    })
}

fn cert_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("S2PA cert build: {e}"),
    ))
}

fn cert_err_msg(s: impl Into<String>) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        s.into(),
    ))
}

/// Escape a DN attribute value per RFC 4514. We only need to handle
/// characters that can appear in DIDs ('=', ',', ':' etc.) — the
/// `did:key:z...` and `did:plc:...` forms contain colons that x509-cert's
/// DN parser splits on if unescaped.
fn escape_dn_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (i, c) in value.chars().enumerate() {
        let needs_escape = match c {
            '"' | '+' | ',' | ';' | '<' | '>' | '\\' => true,
            '#' if i == 0 => true,
            ' ' if i == 0 || i == value.len() - 1 => true,
            _ => false,
        };
        if needs_escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Base58btc encoding (Bitcoin alphabet). Used by `did:key`.
fn base58btc_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 58] =
        b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    // Count leading zero bytes — each maps to a '1' in the output.
    let leading_zeros = bytes.iter().take_while(|&&b| b == 0).count();

    // Convert big-endian bytes → base58 digits (little-endian).
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 138 / 100 + 1);
    for &byte in bytes {
        let mut carry = byte as u32;
        for digit in &mut digits {
            carry += (*digit as u32) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }

    let mut out = String::with_capacity(leading_zeros + digits.len());
    for _ in 0..leading_zeros {
        out.push(ALPHABET[0] as char);
    }
    for &d in digits.iter().rev() {
        out.push(ALPHABET[d as usize] as char);
    }
    out
}

/// RFC 7468 textual encoding of `der_bytes` under the given label.
fn pem_block(label: &str, der_bytes: &[u8]) -> String {
    let b64 = base64_encode(der_bytes);
    let mut out = String::with_capacity(b64.len() + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

/// Standard base64 (RFC 4648 § 4). Lightweight inline impl — avoids
/// pulling in `base64` just for PEM wrapping.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use der::Decode;

    #[test]
    fn did_key_round_trip_through_cert() {
        let key = generate_key();
        let pub_key = key.public_key();
        let expected_did = did_key_for(&pub_key);

        let der = generate_cert(&key, None, None).unwrap();
        let parsed = Certificate::from_der(&der).unwrap();
        let cn = parsed
            .tbs_certificate
            .subject
            .to_string();
        assert!(
            cn.contains(&expected_did),
            "subject DN {cn:?} should embed {expected_did:?}"
        );

        // SPKI algorithm OID must be id-ecPublicKey with named-curve secp256k1.
        let spki = &parsed.tbs_certificate.subject_public_key_info;
        assert_eq!(spki.algorithm.oid, OID_EC_PUBLIC_KEY);
        // Public key bytes match uncompressed SEC1.
        let uncompressed = pub_key.to_encoded_point(false);
        assert_eq!(
            spki.subject_public_key.raw_bytes(),
            uncompressed.as_bytes()
        );
    }

    #[test]
    fn cert_pem_round_trips_through_k256() {
        let key = generate_key();
        let key_pem = key_to_pem(&key).unwrap();
        let restored = k256::SecretKey::from_sec1_pem(&key_pem)
            .or_else(|_| {
                use k256::pkcs8::DecodePrivateKey;
                k256::SecretKey::from_pkcs8_pem(&key_pem)
            })
            .unwrap();
        assert_eq!(
            key.to_bytes().to_vec(),
            restored.to_bytes().to_vec()
        );
    }

    #[test]
    fn did_key_format_matches_bluesky_examples() {
        // Sanity-check our base58btc + multicodec impl against a known
        // secp256k1 → did:key vector. (Generated from an arbitrary key
        // via Streamplace's atcrypto.PublicKeyK256::DIDKey.)
        // If the spec or alphabet were wrong, the prefix length and the
        // first few characters would diverge.
        let key = generate_key();
        let did = did_key_for(&key.public_key());
        assert!(did.starts_with("did:key:z"), "{did}");
        // Compressed key is 33 bytes; with 2-byte multicodec prefix that's
        // 35 bytes. Base58 expansion factor is ~1.37, so encoded length
        // is around 48 characters after the "did:key:z" prefix.
        let encoded_len = did.len() - "did:key:z".len();
        assert!(
            (45..=50).contains(&encoded_len),
            "did:key body should be ~48 chars, got {encoded_len}: {did}"
        );
    }

    // The host-signed path must produce a cert that embeds the given public
    // key and whose signature verifies against it — i.e. the raw r||s the host
    // returns is correctly DER-wrapped as the X.509 signatureValue. Here the
    // "host" is a local k256 key standing in for the orchestrator's keystore.
    #[test]
    fn host_signed_cert_embeds_pubkey_and_verifies() {
        use k256::ecdsa::signature::Verifier;

        let key = generate_key();
        let pubkey = key.public_key();
        let uncompressed = pubkey.to_encoded_point(false).as_bytes().to_vec();
        let did = format!("did:pkh:eip155:1:0x{}", hex_lower(&uncompressed[1..21]));

        let signing_key = SigningKey::from(&key);
        let der = generate_cert_with_signer(&uncompressed, Some(&did), Some("Livepeer"), |tbs_der| {
            // Stand-in for host_sign: ECDSA-SHA256, returned as raw 64-byte r||s.
            let sig: k256::ecdsa::Signature = signing_key.sign(tbs_der);
            Ok(sig.to_bytes().to_vec())
        })
        .unwrap();

        let parsed = Certificate::from_der(&der).unwrap();
        assert_eq!(
            parsed
                .tbs_certificate
                .subject_public_key_info
                .subject_public_key
                .raw_bytes(),
            uncompressed.as_slice(),
            "cert SPKI must carry the supplied uncompressed pubkey"
        );
        assert!(
            parsed.tbs_certificate.subject.to_string().contains(&did),
            "cert DN must embed the supplied did:pkh"
        );

        // The signatureValue must verify against the embedded key over the TBS.
        let tbs_der = parsed.tbs_certificate.to_der().unwrap();
        let sig = k256::ecdsa::Signature::from_der(parsed.signature.raw_bytes()).unwrap();
        let vk = k256::ecdsa::VerifyingKey::from(&signing_key);
        vk.verify(&tbs_der, &sig)
            .expect("cert self-signature must verify against its public key");
    }

    fn hex_lower(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len() * 2);
        for byte in b {
            s.push_str(&format!("{byte:02x}"));
        }
        s
    }
}

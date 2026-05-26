package muxl

import (
	"crypto"
	"crypto/rand"
	"crypto/sha256"
	"encoding/asn1"
	"fmt"
	"math/big"
)

// SignerToCallback adapts a [crypto.Signer] for use as [SignerInput.Sign] /
// [TranscodeInput.Sign].
//
// It hashes data with SHA-256 (matching c2pa's CallbackSigner contract for the
// SHA-256-family algs — ECDSA P-256/secp256k1, RSA PS256), calls the signer,
// and converts ECDSA DER output to the fixed-width r‖s C2PA expects. byteLen is
// the curve's coordinate size: 32 for ES256/ES256K, 48 for ES384, 66 for
// ES512. For RSA-PSS algs byteLen is ignored and the signer output passes
// through.
//
// The signer can be a software ecdsa.PrivateKey, a PKCS#11 device, an Ethereum
// keystore wrapper, etc. — anything implementing [crypto.Signer]. For a
// secp256k1 Ethereum key whose signer returns the 65-byte r‖s‖v recoverable
// form rather than DER, use [RawSignerToCallback] instead.
func SignerToCallback(signer crypto.Signer, byteLen int) func([]byte) ([]byte, error) {
	return func(data []byte) ([]byte, error) {
		digest := sha256.Sum256(data)
		sig, err := signer.Sign(rand.Reader, digest[:], crypto.SHA256)
		if err != nil {
			return nil, fmt.Errorf("muxl host sign: %w", err)
		}
		if raw, ok := derECDSAToRaw(sig, byteLen); ok {
			return raw, nil
		}
		return sig, nil
	}
}

// RawSignerToCallback adapts a digest-signing function that returns a raw
// secp256k1/P-256 signature to a [SignerInput.Sign] / [TranscodeInput.Sign]
// callback. signDigest receives a 32-byte SHA-256 digest and must return
// either the 64-byte r‖s form or the 65-byte r‖s‖v Ethereum recoverable form
// (the trailing recovery byte is dropped). This is the shape an Ethereum
// keystore's SignHash exposes — the basis for a Livepeer orchestrator signing
// S2PA with its existing on-chain identity, no key extraction needed.
//
// byteLen is the coordinate size (32 for ES256K). The returned signature is
// exactly 2*byteLen bytes.
func RawSignerToCallback(signDigest func(digest []byte) ([]byte, error), byteLen int) func([]byte) ([]byte, error) {
	return func(data []byte) ([]byte, error) {
		digest := sha256.Sum256(data)
		sig, err := signDigest(digest[:])
		if err != nil {
			return nil, fmt.Errorf("muxl host sign: %w", err)
		}
		switch len(sig) {
		case 2 * byteLen:
			return sig, nil
		case 2*byteLen + 1: // r‖s‖v — drop the recovery byte.
			return sig[:2*byteLen], nil
		default:
			return nil, fmt.Errorf("muxl host sign: unexpected signature length %d (want %d or %d)", len(sig), 2*byteLen, 2*byteLen+1)
		}
	}
}

// derECDSAToRaw converts a DER-encoded ECDSA signature (SEQUENCE { r, s }, the
// shape crypto.Signer returns for ECDSA keys) to the fixed-width r‖s form
// c2pa expects. r and s are left-padded with zeros to byteLen. Returns
// ok=false if the input isn't ASN.1 DER ECDSA (e.g. an RSA-PSS signature),
// in which case the caller passes the bytes through unchanged.
func derECDSAToRaw(der []byte, byteLen int) ([]byte, bool) {
	var sig struct{ R, S *big.Int }
	rest, err := asn1.Unmarshal(der, &sig)
	if err != nil || len(rest) != 0 || sig.R == nil || sig.S == nil {
		return nil, false
	}
	out := make([]byte, 2*byteLen)
	sig.R.FillBytes(out[:byteLen])
	sig.S.FillBytes(out[byteLen:])
	return out, true
}

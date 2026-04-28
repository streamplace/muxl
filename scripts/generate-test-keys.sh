#!/usr/bin/env bash
# Generate ES256K (secp256k1) test cert chain + private key for muxl-sign
# integration tests. Outputs samples/test-keys/es256k-cert.pem (leaf + CA
# chain) and samples/test-keys/es256k-key.pem (PKCS#8 leaf private key).
#
# These are committed to git so CI doesn't have to regenerate. Run this
# script if the certs expire (current: 100-year validity) or to refresh.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTDIR="$REPO_ROOT/samples/test-keys"
mkdir -p "$OUTDIR"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
cd "$WORKDIR"

# Combined openssl config: req-section for both certs, plus per-cert
# extension sections invoked via -extensions.
cat > openssl.cnf <<'EOF'
[req]
distinguished_name = req_dn
prompt = no

[req_dn]
C = US
O = muxl test
CN = muxl test

[v3_ca]
basicConstraints=critical,CA:TRUE
keyUsage=critical,keyCertSign,digitalSignature
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid:always

[v3_leaf]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=emailProtection
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid:always
EOF

# 1. Self-signed CA — secp256k1, ECDSA-SHA256, valid 100 years.
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:secp256k1 -out ca.key
openssl req -new -x509 -sha256 -key ca.key -out ca.crt \
    -days 36500 \
    -subj "/C=US/O=muxl test/CN=muxl test root" \
    -config openssl.cnf -extensions v3_ca

# 2. Leaf key + CSR + CA-signed leaf cert.
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:secp256k1 -out leaf.key
openssl req -new -sha256 -key leaf.key -out leaf.csr \
    -subj "/C=US/O=muxl test/CN=muxl test signer" \
    -config openssl.cnf
openssl x509 -req -sha256 -in leaf.csr -out leaf.crt \
    -CA ca.crt -CAkey ca.key -CAcreateserial \
    -days 36500 \
    -extfile openssl.cnf -extensions v3_leaf

# 4. Cert chain (leaf first, then CA).
cat leaf.crt ca.crt > "$OUTDIR/es256k-cert.pem"
cp leaf.key "$OUTDIR/es256k-key.pem"

echo "Generated:"
echo "  $OUTDIR/es256k-cert.pem"
echo "  $OUTDIR/es256k-key.pem"
echo
openssl x509 -in "$OUTDIR/es256k-cert.pem" -noout -subject -issuer -dates

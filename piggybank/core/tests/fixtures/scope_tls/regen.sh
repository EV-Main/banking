#!/usr/bin/env bash
# Regenerates every PEM in this directory for tests/scope_ledger_listener.rs: a throwaway CA,
# the scope ledger's server certificate, two service client identities the tests map to
# client ids (`invest`, `panel`), one client the CA signed but no configuration names
# (`stranger`), and a client signed by a second CA nothing here trusts (`foreign`). Each
# client's `*.sha256` is the SHA-256 of its DER certificate, lowercase hex — the
# fingerprint the grants file pins. Ten years of validity; CA keys are not kept.
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

days=3650
key() { openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1" 2>/dev/null; }
ca() {
  key "$tmp/$1.key"
  openssl req -x509 -new -key "$tmp/$1.key" -days "$days" -subj "/CN=$2" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out "$1.pem"
}
# $1 = basename, $2 = signing CA basename, $3 = subject CN, $4 = SAN list, $5 = EKU.
leaf() {
  key "$1.key"
  openssl req -new -key "$1.key" -subj "/CN=$3" -out "$tmp/$1.csr"
  openssl x509 -req -in "$tmp/$1.csr" -CA "$2.pem" -CAkey "$tmp/$2.key" -CAcreateserial -days "$days" \
    -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=%s\nbasicConstraints=CA:FALSE\n' "$4" "$5") \
    -out "$1.pem" 2>/dev/null
  openssl x509 -in "$1.pem" -outform der | openssl dgst -sha256 -r | cut -d' ' -f1 > "$1.sha256"
}

ca ca "EV scope ledger test CA"
ca foreign-ca "Some other CA"
leaf server ca piggybank "DNS:localhost,IP:127.0.0.1" serverAuth
leaf invest ca invest "DNS:invest" clientAuth
leaf panel ca panel "DNS:panel" clientAuth
leaf stranger ca stranger "DNS:stranger" clientAuth
leaf foreign foreign-ca invest "DNS:invest" clientAuth
rm -f server.sha256 ca.srl foreign-ca.srl

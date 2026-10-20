#!/usr/bin/env bash
# Creates a private CA and a server certificate for the CLIProxyAPI TLS sidecar.
#
# Usage: deploy/scripts/cliproxy-tls-certs.sh [OUT_DIR]
#   OUT_DIR  default ~/.config/kanata/cliproxy-tls (created 0700)
# Env: CLIPROXY_TLS_NAMES  extra comma-separated DNS SANs (default: cliproxy-tls)
#      CLIPROXY_TLS_DAYS   server certificate lifetime in days (default 397)
#
# Writes ca.pem (Kanata adapter ca_file), ca.key, server.crt and server.key.
# An existing CA is reused so a renewed server certificate keeps the same trust root.
set -euo pipefail

OUT=${1:-$HOME/.config/kanata/cliproxy-tls}
NAMES=${CLIPROXY_TLS_NAMES:-cliproxy-tls}
DAYS=${CLIPROXY_TLS_DAYS:-397}

die() { echo "error: $*" >&2; exit 1; }
command -v openssl >/dev/null || die "openssl not found"
[[ $DAYS =~ ^[0-9]+$ ]] || die "CLIPROXY_TLS_DAYS must be a number"

umask 077
mkdir -p "$OUT"
chmod 700 "$OUT"

if [[ ! -f $OUT/ca.key || ! -f $OUT/ca.pem ]]; then
  openssl ecparam -name prime256v1 -genkey -noout -out "$OUT/ca.key"
  openssl req -x509 -new -key "$OUT/ca.key" -sha256 -days 3650 \
    -subj "/CN=Kanata CLIProxyAPI sidecar CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out "$OUT/ca.pem"
fi

san="IP:127.0.0.1"
IFS=',' read -ra dns <<<"$NAMES"
for name in "${dns[@]}"; do
  [[ $name =~ ^[A-Za-z0-9.-]+$ ]] || die "invalid DNS name: $name"
  san+=",DNS:$name"
done

openssl ecparam -name prime256v1 -genkey -noout -out "$OUT/server.key"
openssl req -new -key "$OUT/server.key" -subj "/CN=${dns[0]}" -out "$OUT/server.csr"
openssl x509 -req -in "$OUT/server.csr" -CA "$OUT/ca.pem" -CAkey "$OUT/ca.key" \
  -CAcreateserial -sha256 -days "$DAYS" \
  -extfile <(printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=%s\n' "$san") \
  -out "$OUT/server.crt"
rm -f "$OUT/server.csr" "$OUT/ca.srl"
chmod 600 "$OUT"/*.key
chmod 644 "$OUT/ca.pem" "$OUT/server.crt"

echo "wrote $OUT (SAN: $san)"

#!/usr/bin/env bash
#
#  ###################################################################
#  #                                                                 #
#  #   DEVELOPMENT ONLY.  DO NOT USE THE OUTPUT OF THIS SCRIPT IN    #
#  #   PRODUCTION.  It mints its own CA, writes the private key      #
#  #   next to it world-readable-by-owner, and gives everything a    #
#  #   30-day lifetime so that it fails loudly rather than living    #
#  #   on somewhere nobody remembers.                                #
#  #                                                                 #
#  ###################################################################
#
# A script that mints a CA is exactly the artifact that ends up in production,
# and a short expiry is the only mechanism that reliably stops it. That is why
# the lifetime here is deliberately shorter than is convenient.
#
#   ./gen-dev-certs.sh [outdir]     # default: ./tls
#
# Produces, for a server on `localhost`/`127.0.0.1`:
#
#   ca.pem  ca.key           the CA
#   server.pem  server.key   the leader's identity
#   standby-b.pem/.key       a replica's client identity
#   client.pem/.key          an application client's identity
#   ops.pem/.key             an administrator's identity
#   *.sha256                 the DER digest each cert is recognised by
#
# The digests are what go into `[[auth.principal]] cert_sha256`. The digest is
# of the **DER leaf**, not of the PEM file and not of the public key — matching
# on a Subject CN would need an X.509 parser and the CN-versus-SAN distinction is
# a footgun this crate declines to inherit. The cost is that rotating a
# certificate is a config edit.

set -euo pipefail

out="${1:-./tls}"
days=30
mkdir -p "$out"
cd "$out"

command -v openssl >/dev/null || { echo "gen-dev-certs: openssl not found" >&2; exit 1; }

# `rm -f` before every write: this repo's shell notes record that a prezto
# NO_CLOBBER makes `>` fail outright when the target exists.
gen_leaf() {
  local name="$1" san="$2"
  rm -f "$name.key" "$name.csr" "$name.pem" "$name.sha256"
  openssl req -newkey rsa:2048 -nodes -keyout "$name.key" \
    -subj "/CN=$name" -out "$name.csr" 2>/dev/null
  openssl x509 -req -in "$name.csr" -CA ca.pem -CAkey ca.key -CAcreateserial \
    -days "$days" -sha256 -out "$name.pem" \
    -extfile <(printf 'basicConstraints=CA:FALSE\nsubjectAltName=%s\n' "$san") 2>/dev/null
  rm -f "$name.csr"
  # The DER digest yesnod matches a peer certificate on.
  openssl x509 -in "$name.pem" -outform DER \
    | openssl dgst -sha256 -hex | awk '{print $NF}' > "$name.sha256"
  chmod 600 "$name.key"
}

rm -f ca.key ca.pem ca.srl
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem \
  -days "$days" -sha256 -subj "/CN=yesno dev CA (NOT FOR PRODUCTION)" 2>/dev/null
chmod 600 ca.key

gen_leaf server    "DNS:localhost,IP:127.0.0.1"
gen_leaf standby-b "DNS:standby-b"
gen_leaf client    "DNS:client"
gen_leaf ops       "DNS:ops"

echo "wrote $days-day development certificates to $(pwd)"
echo
echo "  [[auth.principal]]"
echo "  name        = \"standby-b\""
echo "  role        = \"replica\""
echo "  cert_sha256 = \"$(cat standby-b.sha256)\""
echo
echo "  [[auth.principal]]"
echo "  name        = \"client\""
echo "  role        = \"writer\""
echo "  cert_sha256 = \"$(cat client.sha256)\""
echo
echo "  [[auth.principal]]"
echo "  name        = \"ops\""
echo "  role        = \"admin\""
echo "  cert_sha256 = \"$(cat ops.sha256)\""
echo
echo "These expire $(date -d "+$days days" +%Y-%m-%d 2>/dev/null || date -v+${days}d +%Y-%m-%d). That is on purpose."

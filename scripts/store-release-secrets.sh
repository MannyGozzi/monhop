#!/usr/bin/env bash
# Stores MonHop's signing secrets in the repository's `release` environment (see docs/SIGNING.md).
# Every input is checked before anything is uploaded. Values go to gh over stdin and are never printed.
# Usage: scripts/store-release-secrets.sh <DeveloperID.p12> <AuthKey_KEYID.p8>
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
env_name=release
updater_key=${MONHOP_UPDATER_KEY:-$HOME/.tauri/monhop-updater.key}
empty_sha256=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855

fail() { echo "$1 Nothing was uploaded." >&2; exit 1; }
[[ $# == 2 ]] || { echo 'Usage: scripts/store-release-secrets.sh <DeveloperID.p12> <AuthKey_KEYID.p8>' >&2; exit 2; }
p12=$1
p8=$2
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

openssl=
for candidate in "$(command -v openssl || true)" /opt/homebrew/bin/openssl /usr/local/bin/openssl; do
  if [[ -x $candidate ]] && "$candidate" version 2>/dev/null | grep -q '^OpenSSL 3'; then
    openssl=$candidate
    break
  fi
done
[[ -n $openssl ]] || fail 'OpenSSL 3 is required (brew install openssl@3).'
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || fail 'gh cannot see the repository; run gh auth login.'

pending=$(gh run list --repo "$repo" --limit 50 --json workflowName,status \
  --jq '[.[] | select((.workflowName == "build" or .workflowName == "release") and .status != "completed")] | length')
[[ $pending == 0 ]] || fail 'A build or release run is queued or running; wait for it to finish.'

[[ -f $updater_key ]] || fail "No updater key at $updater_key (set MONHOP_UPDATER_KEY)."
echo probe > "$scratch/probe"
(cd "$root/apps/monhop-desktop" && cargo tauri signer sign -f "$updater_key" -p '' "$scratch/probe") >/dev/null 2>&1 \
  || fail 'The updater key could not sign a test file (is tauri-cli 2.11.4 installed?).'
python3 -I "$root/scripts/verify_updater_signature.py" "$scratch/probe.sig" >/dev/null \
  || fail 'The updater key does not match the public key built into MonHop.'
echo 'Updater key: matches the public key built into MonHop.'

[[ -f $p8 ]] || fail "No notarization key at $p8."
key_id=$(basename "$p8" .p8); key_id=${key_id#AuthKey_}
[[ $key_id =~ ^[A-Z0-9]{10}$ ]] || fail 'The .p8 must keep its AuthKey_<KEYID>.p8 file name.'
"$openssl" pkey -in "$p8" -noout 2>/dev/null || fail "$p8 is not a readable private key."
echo "Notarization key: Key ID $key_id"

read -rp 'Issuer ID (App Store Connect > Users and Access > Integrations > Team Keys): ' issuer
[[ $issuer =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]] || fail 'That is not an Issuer ID.'

[[ -f $p12 ]] || fail "No certificate at $p12."
for attempt in 1 2 3; do
  read -rsp ".p12 export password (try $attempt of 3): " P12_PASS; echo
  export P12_PASS
  if error=$("$openssl" pkcs12 -legacy -in "$p12" -passin env:P12_PASS -nokeys -noout 2>&1); then
    break
  fi
  echo "  OpenSSL: $(head -1 <<<"$error")" >&2
  [[ $attempt == 3 ]] && fail 'Could not open the .p12.'
done
leaf=$("$openssl" pkcs12 -legacy -in "$p12" -passin env:P12_PASS -nokeys -clcerts 2>/dev/null || true)
identity=$("$openssl" x509 -noout -subject -nameopt multiline <<<"$leaf" 2>/dev/null | sed -n 's/^ *commonName *= //p' || true)
[[ $identity == 'Developer ID Application: '* ]] || fail 'The .p12 does not hold a Developer ID Application certificate.'
key_half=$("$openssl" pkcs12 -legacy -in "$p12" -passin env:P12_PASS -nocerts -nodes 2>/dev/null \
  | "$openssl" pkey -pubout 2>/dev/null | shasum -a 256 | cut -d' ' -f1 || true)
cert_half=$("$openssl" x509 -pubkey -noout <<<"$leaf" 2>/dev/null | shasum -a 256 | cut -d' ' -f1 || true)
[[ $key_half != "$empty_sha256" && $key_half == "$cert_half" ]] || fail 'The .p12 private key is missing or does not match its certificate.'
"$openssl" pkcs12 -legacy -in "$p12" -passin env:P12_PASS -nokeys -cacerts 2>/dev/null | grep -q 'OU *= *G2\|OU=G2' \
  || echo "Warning: the .p12 lacks Apple's G2 intermediate; export it together with the identity (docs/SIGNING.md)." >&2
echo "Certificate: $identity"

set_secret() { gh secret set "$1" --env "$env_name" --repo "$repo" >/dev/null; echo "  set $1"; }
echo "Storing signing secrets in the $env_name environment of $repo:"
base64 -i "$p12" | set_secret APPLE_CERTIFICATE
printf '%s' "$P12_PASS" | set_secret APPLE_CERTIFICATE_PASSWORD
printf '%s' "$identity" | set_secret APPLE_SIGNING_IDENTITY
printf '%s' "$key_id" | set_secret APPLE_API_KEY
printf '%s' "$issuer" | set_secret APPLE_API_ISSUER
set_secret APPLE_API_KEY_P8 < "$p8"
set_secret TAURI_SIGNING_PRIVATE_KEY < "$updater_key"
unset P12_PASS

gh secret list --env "$env_name" --repo "$repo"

#!/usr/bin/env bash
# Export the macOS trust store (System + login keychains) into a directory of
# individual PEM files that vmette mounts into every guest's trust store.
#
# Why: on a machine behind a TLS-inspecting proxy (or with an enterprise CA),
# the intercepting root lives only in the macOS keychain. Guests don't see it,
# so every HTTPS call inside a sandbox fails with a cert-authority error. This
# stages those roots where vmette looks for them by default
# (`~/.config/vmette/certs`), so `execute` / `fetch_url` / `workspace_run` /
# `desktop_*` and the `vmette` CLI all trust them with no per-call flags.
#
# Each cert is written to its OWN file (`cert-NNN.pem`) rather than one bundle:
# the guest-side installer concatenates them, and per-file output sidesteps the
# multi-cert-bundle parsing pitfalls some tools have.
#
# Works on both Apple Silicon and Intel Macs — `security` and the System Roots
# keychain path are identical across architectures.
#
# Usage: scripts/export-macos-ca-certs.sh [OUTPUT_DIR]
#   OUTPUT_DIR defaults to $VMETTE_CA_CERTS, else ~/.config/vmette/certs.

set -euo pipefail

OUT_DIR="${1:-${VMETTE_CA_CERTS:-$HOME/.config/vmette/certs}}"

# The keychains worth exporting: the immutable system roots (Apple + anything
# MDM/admin injected there) and the admin-scoped trust store. The per-user
# login keychain often holds proxy roots installed by IT tooling too.
KEYCHAINS=(
    "/System/Library/Keychains/SystemRootCertificates.keychain"
    "/Library/Keychains/System.keychain"
    "$HOME/Library/Keychains/login.keychain-db"
)

mkdir -p "$OUT_DIR"
# Clear stale exports so a removed/rotated root doesn't linger in the guest.
rm -f "$OUT_DIR"/cert-*.pem 2>/dev/null || true

tmp_all="$(mktemp)"
trap 'rm -f "$tmp_all"' EXIT

for kc in "${KEYCHAINS[@]}"; do
    [[ -e "$kc" ]] || continue
    # `security find-certificate -a -p` prints every cert in the keychain as a
    # concatenated PEM stream. Tolerate keychains we can't read (e.g. a locked
    # login keychain) rather than aborting the whole export.
    security find-certificate -a -p "$kc" >> "$tmp_all" 2>/dev/null || true
done

if [[ ! -s "$tmp_all" ]]; then
    echo "✗ no certificates exported (could not read any keychain)" >&2
    exit 1
fi

# Split the concatenated stream into one file per certificate. awk emits a new
# file at each BEGIN marker; the trailing END line is included by the range.
count="$(awk -v out="$OUT_DIR" '
    /-----BEGIN CERTIFICATE-----/ { n++; f = sprintf("%s/cert-%03d.pem", out, n) }
    n > 0 { print > f }
    /-----END CERTIFICATE-----/   { close(f) }
    END { print n+0 }
' "$tmp_all")"

if [[ "$count" -eq 0 ]]; then
    echo "✗ stream held no PEM certificates" >&2
    exit 1
fi

echo "✓ exported $count certificate(s) to $OUT_DIR"
echo "  vmette now trusts these in every guest (no flags needed)."
if [[ "$OUT_DIR" != "$HOME/.config/vmette/certs" && -z "${VMETTE_CA_CERTS:-}" ]]; then
    echo "  NOTE: this is not the default dir — set VMETTE_CA_CERTS=$OUT_DIR"
fi

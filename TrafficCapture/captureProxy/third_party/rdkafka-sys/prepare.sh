#!/usr/bin/env bash
# Produce the patched rdkafka-sys crate that the workspace's [patch.crates-io] points at.
#
# Downloads exactly rdkafka-sys 4.10.0+2.12.1 from crates.io, verifies it against the checksum
# Cargo.lock pins for the unpatched crate, extracts it to ./crate (git-ignored), and applies the
# patches in ./patches in order. Idempotent: a matching ./crate is left alone.
#
# Run once before any cargo command (the Dockerfile runs it in the builder stage).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NAME=rdkafka-sys
VERSION=4.10.0+2.12.1
SHA256=e234cf318915c1059d4921ef7f75616b5219b10b46e9f3a511a15eb4b56a3f77
URL="https://static.crates.io/crates/${NAME}/${NAME}-${VERSION}.crate"
DEST="$HERE/crate"
STAMP="$DEST/.capture-proxy-patches"

want_stamp() { (echo "$SHA256"; cd "$HERE/patches" && for p in *.patch; do printf '%s ' "$p"; shasum -a 256 "$p" 2>/dev/null | cut -d' ' -f1 || sha256sum "$p" | cut -d' ' -f1; done); }
sha256_of() { shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1 || sha256sum "$1" | cut -d' ' -f1; }

if [[ -f "$STAMP" ]] && diff -q <(want_stamp) "$STAMP" >/dev/null; then
  echo "rdkafka-sys: patched crate up to date at $DEST"
  exit 0
fi

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
curl --proto '=https' --tlsv1.2 -sSfL --retry 3 -o "$WORK/crate.tar.gz" "$URL"
GOT="$(sha256_of "$WORK/crate.tar.gz")"
if [[ "$GOT" != "$SHA256" ]]; then
  echo "rdkafka-sys: checksum mismatch: got $GOT, want $SHA256" >&2
  exit 1
fi
mkdir -p "$WORK/x"
tar -xzf "$WORK/crate.tar.gz" -C "$WORK/x"
SRC="$WORK/x/${NAME}-${VERSION}"
for p in "$HERE"/patches/*.patch; do
  echo "rdkafka-sys: applying $(basename "$p")"
  (cd "$SRC" && patch -s -p1 --forward --no-backup-if-mismatch < "$p")
done
rm -rf "$DEST"
mv "$SRC" "$DEST"
want_stamp > "$STAMP"
echo "rdkafka-sys: patched crate ready at $DEST"

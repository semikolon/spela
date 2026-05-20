#!/usr/bin/env bash
# Mac release build with stable-identifier codesign so TCC grants (Full
# Disk Access / Removable Volumes) survive rebuilds.
#
# Why this exists: Apple's TCC keys grants to the binary's code-signature
# hash. Cargo's `linker-signed` ad-hoc default identifier is content-
# derived (e.g. `spela-56c693bdcecc486f`) — every rebuild flips it, every
# rebuild silently revokes the FDA grant, every rebuild surprises Fredrik
# when `serve-library`'s `read_dir(BOHR)` hangs ("this happened again").
#
# This wrapper re-signs the freshly-built binary with a STABLE
# identifier (`com.fredrikbranstrom.spela`) so the signature's identity
# field carries across rebuilds. Whether TCC honours identifier-equality
# for ad-hoc-signed binaries (no Apple Developer cert anchoring the
# identifier) is implementation-defined — but the TODO entry pinned this
# as the durability fix per global CLAUDE.md "TCC code signature gotcha",
# so we ship it and observe across the next few rebuilds.
#
# ONE-TIME re-grant required after the FIRST run of this wrapper: the
# new stable identifier differs from the old hash-based one, so TCC will
# revoke the existing FDA grant once. Add the binary back via System
# Settings → Privacy & Security → Full Disk Access → +, then `launchctl
# kickstart -k gui/$(id -u)/com.fredrikbranstrom.spela-library`. From
# the SECOND run onwards, the grant should survive.
#
# Usage: ./bin/build-mac.sh [extra cargo args]
# Example: ./bin/build-mac.sh                  # release build + codesign
#          ./bin/build-mac.sh --no-default-features  # extra flags forwarded

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$REPO_ROOT/target/release/spela"
STABLE_IDENTIFIER="com.fredrikbranstrom.spela"

cd "$REPO_ROOT"

echo "==> cargo build --release $*"
cargo build --release "$@"

echo "==> ad-hoc codesign with stable identifier"
codesign \
  --sign - \
  --identifier "$STABLE_IDENTIFIER" \
  --force \
  "$BINARY"

# Verify (defense-in-depth): the binary's identifier should be the
# stable string we just set, NOT the cargo linker-signed default.
ACTUAL_ID=$(codesign -d --verbose=4 "$BINARY" 2>&1 | awk -F= '/^Identifier=/ {print $2}')
if [[ "$ACTUAL_ID" != "$STABLE_IDENTIFIER" ]]; then
  echo "ERROR: post-codesign identifier is '$ACTUAL_ID', expected '$STABLE_IDENTIFIER'" >&2
  exit 1
fi

echo "==> built + signed: $BINARY"
echo "    identifier: $ACTUAL_ID"
echo "    next step:  launchctl kickstart -k gui/\$(id -u)/com.fredrikbranstrom.spela-library"
echo "                (verify FDA grant survives — if /library/list hangs,"
echo "                 System Settings → Privacy & Security → Full Disk Access → re-Allow)"

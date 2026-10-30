#!/usr/bin/env bash
# Rebuild a .deb with an xz-compressed data tarball.
#
# tauri-bundler writes `data.tar.gz` with gzip level 6 regardless of the
# payload, even though every supported dpkg reads xz. DBX's 103 MB binary
# shrinks from ~49 MB to ~36 MB with `dpkg-deb -Zxz`.
set -euo pipefail

deb=${1:?usage: repack-deb.sh <path/to/*.deb>}
[[ -f "$deb" ]] || { echo "repack-deb: not a file: $deb" >&2; exit 1; }

before=$(wc -c <"$deb")
work=$(mktemp -d "${TMPDIR:-/tmp}/dbx-deb-repack.XXXXXX")
trap 'rm -rf "$work"' EXIT

dpkg-deb -R "$deb" "$work/root"
rm -f "$deb"
# --root-owner-group keeps the archive deterministic when not running as root.
dpkg-deb --root-owner-group -Zxz -z6 --build "$work/root" "$deb" >/dev/null

after=$(wc -c <"$deb")
echo "repack-deb: $deb $before -> $after bytes"

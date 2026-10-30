#!/usr/bin/env bash
# Convert a Tauri .dmg from UDZO (zlib) to ULMO (LZMA) in place.
#
# tauri-bundler calls its embedded bundle_dmg script without --format, which
# hardcodes UDZO. ULMO is ~18% smaller on DBX's 96 MB binary (49.4 MB ->
# 40.6 MB) and is the only hdiutil format that beats UDZO here; ULFO and UDBZ
# were both measured worse or barely better.
#
# The .dmg signature is invalidated by the conversion, so the caller must
# re-sign (and re-notarize) the image afterwards.
set -euo pipefail

dmg=${1:?usage: convert-dmg-ulmo.sh <path/to/*.dmg>}
[[ -f "$dmg" ]] || { echo "convert-dmg-ulmo: not a file: $dmg" >&2; exit 1; }

before=$(wc -c <"$dmg")
work=$(mktemp -d "${TMPDIR:-/tmp}/dbx-dmg-ulmo.XXXXXX")
trap 'rm -rf "$work"' EXIT

hdiutil convert "$dmg" -format ULMO -o "$work/converted.dmg" -ov -quiet
[[ -f "$work/converted.dmg" ]] || { echo "convert-dmg-ulmo: conversion failed" >&2; exit 1; }

mv "$work/converted.dmg" "$dmg"
after=$(wc -c <"$dmg")
echo "convert-dmg-ulmo: $dmg $before -> $after bytes"

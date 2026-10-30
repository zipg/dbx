#!/usr/bin/env bash
# Recompress a Tauri AppImage in place.
#
# Tauri asks linuxdeploy-plugin-appimage for an AppImage without passing any
# compressor options, so appimagetool falls back to `mksquashfs -comp zstd
# -b 128K` with no level. Repacking the already-extracted AppDir with
# `xz -b 1M -Xbcj x86` shrinks the image by roughly 18% on DBX's payload
# (123 MB -> 100 MB) at the cost of a slower cold start.
#
# The AppDir is extracted with unsquashfs (no execution), so an arm64 image can
# be repacked on an x64 runner. The AppImage runtime architecture is derived
# from the bundled dbx ELF header and passed to appimagetool via ARCH.
#
# Requires: appimagetool on PATH (or APPIMAGETOOL=/path), unsquashfs, python3.
set -euo pipefail

appimage=${1:?usage: repack-appimage.sh <path/to/*.AppImage>}
tool=${APPIMAGETOOL:-appimagetool}

if [[ ! -f "$appimage" ]]; then
  echo "repack-appimage: not a file: $appimage" >&2
  exit 1
fi

before=$(wc -c <"$appimage")
work=$(mktemp -d "${TMPDIR:-/tmp}/dbx-appimage-repack.XXXXXX")
trap 'rm -rf "$work"' EXIT

offset=$(python3 - "$appimage" <<'PY'
import struct
import sys

path = sys.argv[1]
data = open(path, "rb").read(8 * 1024 * 1024)
pos = 0
while True:
    idx = data.find(b"hsqs", pos)
    if idx < 0:
        raise SystemExit("no squashfs superblock found")
    if idx + 96 <= len(data):
        _magic, _inodes, _mkfs, block, _frags, comp = struct.unpack("<IIIIIH", data[idx:idx + 22])
        if block and block & (block - 1) == 0 and 1 <= comp <= 6:
            print(idx)
            break
    pos = idx + 4
PY
)

unsquashfs -q -o "$offset" -d "$work/squashfs-root" "$appimage" >/dev/null

runtime_arch=$(python3 - "$work/squashfs-root/usr/bin/dbx" <<'PY'
import struct
import sys

with open(sys.argv[1], "rb") as handle:
    header = handle.read(20)
if header[:4] != b"\x7fELF":
    raise SystemExit("not an ELF binary")
machine = struct.unpack("<H", header[18:20])[0]
print({0x3E: "x86_64", 0xB7: "aarch64"}.get(machine, "x86_64"))
PY
)

output="$work/repacked.AppImage"
mksquashfs_opts=(--mksquashfs-opt -b --mksquashfs-opt 1M)
# BCJ is a per-architecture filter; only x86-64 payloads benefit.
if [[ "$runtime_arch" == "x86_64" ]]; then
  mksquashfs_opts+=(--mksquashfs-opt -Xbcj --mksquashfs-opt x86)
fi
ARCH="$runtime_arch" APPIMAGE_EXTRACT_AND_RUN=1 "$tool" "$work/squashfs-root" "$output" \
  --comp xz \
  "${mksquashfs_opts[@]}" \
  --no-appstream >/dev/null

mv "$output" "$appimage"
chmod +x "$appimage"

after=$(wc -c <"$appimage")
echo "repack-appimage: $appimage ($runtime_arch) $before -> $after bytes"

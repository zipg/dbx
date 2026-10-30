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
# be repacked on an x64 runner. The original runtime is reused via
# --runtime-file, and appimagetool's own bundled mksquashfs (zstd-only) is
# dropped so the distro mksquashfs with xz support is used.
#
# Requires: appimagetool (binary or .AppImage), unsquashfs, mksquashfs with xz,
# python3.
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

data = open(sys.argv[1], "rb").read(8 * 1024 * 1024)
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

unsquashfs -q -o "$offset" -d "$work/appdir" "$appimage" >/dev/null
head -c "$offset" "$appimage" >"$work/runtime"

runtime_arch=$(python3 - "$work/appdir/usr/bin/dbx" <<'PY'
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

tool_bin=$tool
path_prefix=""
if [[ "$tool" == *.AppImage && -f "$tool" ]]; then
  # appimagetool ships its own zstd-only mksquashfs; extract the tool and drop
  # that binary so PATH resolves the distro mksquashfs (which has xz).
  mkdir -p "$work/tool"
  (cd "$work/tool" && APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --appimage-extract >/dev/null)
  rm -f "$work/tool/squashfs-root/usr/bin/mksquashfs"
  tool_bin="$work/tool/squashfs-root/usr/bin/appimagetool"
  path_prefix="$work/tool/squashfs-root/usr/bin:"
fi

mksquashfs_opts=(--mksquashfs-opt -b --mksquashfs-opt 1M)
# BCJ is a per-architecture filter; only x86-64 payloads benefit.
if [[ "$runtime_arch" == "x86_64" ]]; then
  mksquashfs_opts+=(--mksquashfs-opt -Xbcj --mksquashfs-opt x86)
fi

output="$work/repacked.AppImage"
PATH="${path_prefix}/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" \
  ARCH="$runtime_arch" "$tool_bin" \
  --comp xz \
  "${mksquashfs_opts[@]}" \
  --runtime-file "$work/runtime" \
  --no-appstream \
  "$work/appdir" "$output" >/dev/null

mv "$output" "$appimage"
chmod +x "$appimage"

after=$(wc -c <"$appimage")
echo "repack-appimage: $appimage ($runtime_arch) $before -> $after bytes"

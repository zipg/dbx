# Release artifact shrinking

Tauri's bundlers pick conservative compression defaults; these scripts re-pack the
already-built bundles without touching application code. They are wired into
`.github/workflows/release.yml`:

| script | artifact | change | measured on 0.6.61 |
|---|---|---|---|
| `repack-appimage.sh` | `.AppImage` | squashfs `zstd -b 128K` → `xz -b 1M` (+ BCJ on x86-64) | 123.2 MB → 100.7 MB |
| `repack-deb.sh` | `.deb` | `data.tar.gz` (gzip-6) → `data.tar.xz` | 49.0 MB → ~36.4 MB |
| `convert-dmg-ulmo.sh` | `.dmg` | UDZO (zlib-9) → ULMO (LZMA) | 49.4 MB → 40.6 MB |
| `patch-latest-json.mjs` | `latest.json` | refresh updater signatures after repacking | — |

`rpm` compression is a Tauri config knob (`bundle.linux.rpm.compression`) and is
set in `src-tauri/tauri.conf.json`; the NSIS dictionary size lives in
`src-tauri/windows/nsis/installer.nsi`.

Notes:

- Repacking changes the file bytes, so `latest.json` signatures must be
  regenerated (`tauri signer sign`) and patched. The workflow uploads the new
  `.sig` files, patches `latest.json`, and lets the existing cleanup job delete
  the standalone signatures afterwards.
- The DMG conversion invalidates the DMG's own signature; the workflow re-signs
  and re-notarizes the converted image. The `.app` inside is untouched.
- `repack-appimage.sh` extracts with `unsquashfs -o <offset>` instead of running
  the AppImage, so arm64 images can be repacked on x64 runners. `ARCH` is derived
  from the bundled `dbx` ELF header.

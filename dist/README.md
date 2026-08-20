# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/komet-<version>-linux-<arch>.tar.gz` containing:

- `komet` — the binary (headed by default; `komet headless` runs the engine alone)
- `komet.desktop` — XDG desktop entry
- `komet.png` — 1024×1024 Komet app icon
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/komet-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Komet.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. The auto-update tarball retains an internal `Komet.app` path so older
installed builds can update into Komet. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host — gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p komet --target aarch64-apple-darwin
   cargo build --release -p komet --target x86_64-apple-darwin
   lipo -create -output komet \
     target/aarch64-apple-darwin/release/komet \
     target/x86_64-apple-darwin/release/komet
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Komet.app/Contents/{MacOS,Resources}
   cp komet Komet.app/Contents/MacOS/komet
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Komet.app/Contents/Info.plist
   ```
3. Icon: generate `komet.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Komet.app/Contents/Resources/komet.icns`:
   ```sh
   mkdir komet.iconset && sips -z 256 256 dist/macos/icon-1024.png --out komet.iconset/icon_256x256.png
   iconutil -c icns komet.iconset -o Komet.app/Contents/Resources/komet.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Komet.app
   xcrun notarytool submit Komet.zip --keychain-profile … --wait
   xcrun stapler staple Komet.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Komet -srcfolder Komet.app -ov -format UDZO Komet.dmg`).

#!/usr/bin/env bash
# Build RemoteCraft-macos.dmg locally on a Mac, mirroring the macOS job in
# .github/workflows/release.yml. Use this when GitHub's macOS runners are
# unavailable (e.g. the private repo's macOS Actions minutes are used up).
#
# Requirements: macOS, Rust (https://rustup.rs). Run from the repo root:
#     bash packaging/macos/build-dmg.sh
# The finished image is written to dist/RemoteCraft-macos.dmg.
set -euo pipefail

cd "$(dirname "$0")/../.."

# Version for the .app bundle: the current git tag if on one, else the
# workspace version from Cargo.toml.
VERSION="$(git describe --tags --exact-match 2>/dev/null || true)"
VERSION="${VERSION#v}"
if [ -z "$VERSION" ]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
fi
[ -z "$VERSION" ] && VERSION="0.0.0"
echo "Building RemoteCraft $VERSION for macOS (universal)…"

rustup target add x86_64-apple-darwin aarch64-apple-darwin

cargo build --release --target x86_64-apple-darwin -p remotecraft -p remotecraft-relay
cargo build --release --target aarch64-apple-darwin -p remotecraft -p remotecraft-relay

rm -rf dist/dmg
mkdir -p dist/dmg
lipo -create -output dist/dmg/remotecraft \
  target/x86_64-apple-darwin/release/remotecraft \
  target/aarch64-apple-darwin/release/remotecraft
lipo -create -output dist/dmg/remotecraft-relay \
  target/x86_64-apple-darwin/release/remotecraft-relay \
  target/aarch64-apple-darwin/release/remotecraft-relay
cp packaging/macos/README.txt dist/dmg/README.txt

# Assemble the double-clickable app bundle.
APP="dist/dmg/RemoteCraft.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp dist/dmg/remotecraft "$APP/Contents/MacOS/remotecraft"
cp dist/dmg/remotecraft-relay "$APP/Contents/MacOS/remotecraft-relay"
chmod +x "$APP/Contents/MacOS/remotecraft" "$APP/Contents/MacOS/remotecraft-relay"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

hdiutil create -volname RemoteCraft -srcfolder dist/dmg \
  -ov -format UDZO dist/RemoteCraft-macos.dmg

echo "Done: dist/RemoteCraft-macos.dmg"

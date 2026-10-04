#!/bin/sh
# Build "Hex.app" using the persistent release identity. For isolated UI
# development use cargo run -- preview; never replace the app with an ad hoc build.
#
# Usage: scripts/build-app.sh [--prepare]
# --prepare builds the bundle only; code_signing.py signs and packages it.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
prepare=false
case "$#:${1:-}" in
  0:) ;;
  1:--prepare) prepare=true ;;
  *) echo "Usage: scripts/build-app.sh [--prepare]" >&2; exit 1 ;;
esac
version=$(python3 "$root/scripts/release.py" version)
identity=${HEX_CODESIGN_IDENTITY:--}
if [ "${GITHUB_ACTIONS:-}" = "true" ] && [ "$prepare" = false ]; then
  echo "Releases require the pinned signing identity; refusing ad hoc signing." >&2
  exit 1
fi
if [ "$prepare" = false ] && [ "$identity" = "-" ]; then
  echo "Set HEX_CODESIGN_IDENTITY to the pinned release certificate; refusing ad hoc signing." >&2
  echo "For isolated UI development use: cargo run -- preview settings" >&2
  exit 1
fi

bundle_name="Hex.app"
bundle="$root/target/app/$bundle_name"
plist="$bundle/Contents/Info.plist"
icon_output="$root/target/AppIcon.assets"

export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --locked --release --manifest-path "$root/Cargo.toml" >&2

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$root/target/release/voice-control" "$bundle/Contents/MacOS/hex"
cp "$root/app/Info.plist" "$plist"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$bundle/Contents/Resources/"
/usr/bin/plutil -replace CFBundleShortVersionString -string "$version" "$plist"
/usr/bin/plutil -replace CFBundleVersion -string "$version" "$plist"

rm -rf "$icon_output"
mkdir -p "$icon_output"
xcrun actool "$root/app/AppIcon.icon" \
  --compile "$icon_output" \
  --platform macosx \
  --minimum-deployment-target 15.0 \
  --app-icon AppIcon \
  --standalone-icon-behavior all \
  --output-partial-info-plist "$root/target/AppIcon-info.plist" \
  --output-format human-readable-text >&2
cp "$icon_output/AppIcon.icns" "$icon_output/Assets.car" "$bundle/Contents/Resources/"

if [ "$prepare" = true ]; then
  echo "$bundle"
  exit 0
fi

set -- --force --sign "$identity" --entitlements "$root/app/VoiceControl.entitlements"
if [ -n "${HEX_CODESIGN_KEYCHAIN:-}" ]; then
  set -- "$@" --keychain "$HEX_CODESIGN_KEYCHAIN" --timestamp=none
fi
codesign "$@" "$bundle" >&2
codesign --verify --deep --strict --verbose=2 "$bundle" >&2
python3 "$root/scripts/code_signing.py" --verify-bundle "$bundle"

archive="$root/target/app/Hex-$version.zip"
rm -f "$archive"
(cd "$root/target/app" && /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$bundle_name" "$archive")
echo "$archive"

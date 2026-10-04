#!/bin/sh
# Build "Hex.app": no Developer ID, no Sparkle (Homebrew updates it).
# Signs ad hoc by default, or with HEX_CODESIGN_IDENTITY when set (a stable
# identity keeps macOS permission grants across updates).
#
# Usage: scripts/build-app.sh   -> prints the path of the .zip it built
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
[ "$#" -eq 0 ] || { echo "Usage: scripts/build-app.sh" >&2; exit 1; }
version=$(python3 "$root/scripts/release.py" version)
identity=${HEX_CODESIGN_IDENTITY:--}

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

codesign --force --sign "$identity" --entitlements "$root/app/VoiceControl.entitlements" "$bundle" >&2
codesign --verify --deep --strict --verbose=2 "$bundle" >&2

archive="$root/target/app/Hex-$version.zip"
rm -f "$archive"
(cd "$root/target/app" && /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$bundle_name" "$archive")
echo "$archive"

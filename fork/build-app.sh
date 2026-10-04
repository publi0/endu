#!/bin/sh
# Build "Hex OpenRouter.app": no Developer ID, no Sparkle (Homebrew updates it).
# Signs ad hoc by default, or with FORK_CODESIGN_IDENTITY when set (a stable
# identity keeps macOS permission grants across updates).
#
# Usage: fork/build-app.sh [version]   -> prints the path of the .zip it built
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
crate_version=$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" | jq -r '.packages[0].version')
version=${1:-$crate_version}
build_number=$(printf '%s\n' "$crate_version" | awk -F. '{ print ($1 * 10000) + ($2 * 100) + $3 }')
identity=${FORK_CODESIGN_IDENTITY:--}

bundle_name="Hex OpenRouter.app"
bundle="$root/target/fork-app/$bundle_name"
plist="$bundle/Contents/Info.plist"
icon_output="$root/target/fork-AppIcon.assets"

export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --locked --release --manifest-path "$root/Cargo.toml" >&2

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$root/target/release/voice-control" "$bundle/Contents/MacOS/hex"
cp "$root/app/Info.plist" "$plist"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$bundle/Contents/Resources/"
/usr/bin/plutil -replace CFBundleShortVersionString -string "$version" "$plist"
/usr/bin/plutil -replace CFBundleVersion -string "$build_number" "$plist"

rm -rf "$icon_output"
mkdir -p "$icon_output"
xcrun actool "$root/app/AppIcon.icon" \
  --compile "$icon_output" \
  --platform macosx \
  --minimum-deployment-target 15.0 \
  --app-icon AppIcon \
  --standalone-icon-behavior all \
  --output-partial-info-plist "$root/target/fork-AppIcon-info.plist" \
  --output-format human-readable-text >&2
cp "$icon_output/AppIcon.icns" "$icon_output/Assets.car" "$bundle/Contents/Resources/"

codesign --force --sign "$identity" --entitlements "$root/app/VoiceControl.entitlements" "$bundle" >&2
codesign --verify --deep --strict --verbose=2 "$bundle" >&2

archive="$root/target/fork-app/Hex-OpenRouter-$version.zip"
rm -f "$archive"
(cd "$root/target/fork-app" && /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$bundle_name" "$archive")
echo "$archive"

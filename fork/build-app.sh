#!/bin/sh
# Build "Hex OpenRouter.app" from this fork without Developer ID or Sparkle.
#
# Mirrors scripts/build-app.sh but:
#   - compiles with `--features openrouter`;
#   - uses its own name and bundle identifier, so it installs beside upstream
#     HEX and never receives upstream Sparkle updates (no feed, no framework);
#   - signs ad hoc by default, or with FORK_CODESIGN_IDENTITY when set (a
#     stable identity keeps macOS permission grants across updates).
#
# Usage: fork/build-app.sh [version]   -> prints the path of the .zip it built
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
upstream_version=$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" | jq -r '.packages[0].version')
version=${1:-$upstream_version}
build_number=$(printf '%s\n' "$upstream_version" | awk -F. '{ print ($1 * 10000) + ($2 * 100) + $3 }')
identity=${FORK_CODESIGN_IDENTITY:--}

bundle_name="Hex OpenRouter.app"
bundle_id=dev.publio.hex-openrouter
display_name="Hex OpenRouter"
executable_name=hex

bundle="$root/target/fork-app/$bundle_name"
executable="$bundle/Contents/MacOS/$executable_name"
frameworks="$bundle/Contents/Frameworks"
plist="$bundle/Contents/Info.plist"
icon_output="$root/target/fork-AppIcon.assets"

export MACOSX_DEPLOYMENT_TARGET=15.0
cargo build --locked --release --features openrouter --manifest-path "$root/Cargo.toml" >&2

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources" "$frameworks"
cp "$root/target/release/voice-control" "$executable"
cp "$root/app/Info.plist" "$plist"
cp "$root/LICENSE" "$root/THIRD_PARTY_NOTICES.md" "$bundle/Contents/Resources/"

(cd "$root/sdk" && bun install --frozen-lockfile >&2)
(cd "$root/sdk/commands" && bun run build >&2)
mkdir -p "$bundle/Contents/Resources/commands-sdk"
cp "$root/sdk/commands/package.json" "$bundle/Contents/Resources/commands-sdk/"
/usr/bin/ditto "$root/sdk/commands/dist" "$bundle/Contents/Resources/commands-sdk/dist"
/usr/bin/ditto "$root/sdk/commands/workspace-template" "$bundle/Contents/Resources/commands-sdk/workspace-template"

set_string() { /usr/bin/plutil -replace "$1" -string "$2" "$plist"; }
set_string CFBundleShortVersionString "$version"
set_string CFBundleVersion "$build_number"
set_string CFBundleIdentifier "$bundle_id"
set_string CFBundleExecutable "$executable_name"
set_string CFBundleName "$display_name"
set_string CFBundleDisplayName "$display_name"
set_string NSMicrophoneUsageDescription "HEX uses your microphone for dictation. With OpenRouter selected, each recording is sent to OpenRouter for transcription and is not stored."
for key in SUFeedURL SUPublicEDKey SUEnableAutomaticChecks SUAutomaticallyUpdate SUAllowsAutomaticUpdates SUVerifyUpdateBeforeExtraction; do
  /usr/bin/plutil -remove "$key" "$plist" 2>/dev/null || true
done

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

# Moonshine powers the opt-in voice Commands; scripts/setup.sh installs it.
set -- "$root"/.venv/lib/python*/site-packages/moonshine_voice
if [ -d "$1" ]; then
  cp "$1/libmoonshine.dylib" "$frameworks/"
  cp "$1"/libonnxruntime.*.dylib "$frameworks/"
else
  echo "warning: Moonshine not installed; voice Commands will be unavailable" >&2
fi

# Ad hoc signatures have no Team ID, so skip the hardened runtime: its library
# validation would refuse the bundled dylibs.
for library in "$frameworks"/*.dylib; do
  [ -e "$library" ] && codesign --force --sign "$identity" "$library" >&2
done
codesign --force --sign "$identity" --entitlements "$root/app/VoiceControl.entitlements" "$bundle" >&2
codesign --verify --deep --strict --verbose=2 "$bundle" >&2

archive="$root/target/fork-app/Hex-OpenRouter-$version.zip"
rm -f "$archive"
(cd "$root/target/fork-app" && /usr/bin/ditto -c -k --sequesterRsrc --keepParent "$bundle_name" "$archive")
echo "$archive"

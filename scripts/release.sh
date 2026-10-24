#!/bin/bash
# Build a release of Berth.app: target/dist/Berth-<version>.zip and its
# .sha256. Signed with a Developer ID and the hardened runtime, notarized
# and stapled, when these are set; otherwise ad-hoc signed, which only
# opens on the Mac that built it.
#
#   BERTH_SIGN_IDENTITY   "Developer ID Application: Name (TEAMID)", in the keychain
#   BERTH_NOTARY_PROFILE  a notarytool keychain profile, made once with
#                         xcrun notarytool store-credentials <profile> --apple-id ... --team-id ...
#
# The version comes from Cargo.toml, the build number from the commit count.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
identity="${BERTH_SIGN_IDENTITY:-}"
profile="${BERTH_NOTARY_PROFILE:-}"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
build=$(git -C "$root" rev-list --count HEAD)
"$root/scripts/build-app.sh"

dist="$root/target/dist"
app="$dist/Berth.app"
zip="$dist/Berth-$version.zip"
rm -rf "$dist"
mkdir -p "$dist"
ditto "$root/target/Berth.app" "$app"
plist="$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build" "$plist"

if [ -n "$identity" ]; then
    # Inner binary first (it needs the virtualization entitlement), then the
    # bundle. Hardened runtime and a secure timestamp: notarization needs both.
    codesign --force --options runtime --timestamp --sign "$identity" \
        --entitlements "$root/berth.entitlements" "$app/Contents/Helpers/berth"
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
    codesign --verify --strict --deep --verbose=2 "$app"
else
    codesign --force --sign - --entitlements "$root/berth.entitlements" "$app/Contents/Helpers/berth"
    codesign --force --sign - "$app"
    echo "warning: BERTH_SIGN_IDENTITY isn't set: ad-hoc signed, for this Mac only" >&2
fi
ditto -c -k --keepParent "$app" "$zip"

if [ -n "$identity" ] && [ -n "$profile" ]; then
    xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait
    # The ticket goes into the app, so it opens offline too; zip it again.
    xcrun stapler staple "$app"
    rm "$zip"
    ditto -c -k --keepParent "$app" "$zip"
    spctl --assess --type execute --verbose=2 "$app"
elif [ -n "$identity" ]; then
    echo "warning: BERTH_NOTARY_PROFILE isn't set: signed but not notarized; Gatekeeper will refuse it" >&2
fi

(cd "$dist" && shasum -a 256 "$(basename "$zip")" > "$(basename "$zip").sha256")
echo "built $zip ($version, build $build)"

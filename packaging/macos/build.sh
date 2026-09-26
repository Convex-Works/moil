#!/usr/bin/env bash
# Builds target/macos/Moil.app for this Mac's architecture: release builds
# of moil-app and the moil CLI, a pinned uv (checked against its SHA-256),
# the app icon, all ad-hoc signed.
#
#   packaging/macos/build.sh
#
# Needs Rust, the Xcode command line tools (swift, iconutil, codesign) and
# network access the first time, to download uv.
set -euo pipefail

# uv, pinned. To update, take the version and the SHA-256 of each
# uv-<arch>-apple-darwin.tar.gz from https://github.com/astral-sh/uv/releases.
UV_VERSION=0.12.19
UV_SHA256_aarch64=a9a8df1eedeb192f2e47e40e2faabfb387db4b850209118786d42f89dde3e0ba
UV_SHA256_x86_64=cb5fa57bafe68fc0fb94b17f06bee0b0b9a7feb94ccbd110445afa0696e39273

root="$(cd "$(dirname "$0")/../.." && pwd)"
here="$root/packaging/macos"
out="$root/target/macos"
app="$out/Moil.app"

case "$(uname -m)" in
    arm64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) echo "build.sh: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

# "path+file:///…/crates/moil-app#0.1.0" or "…#moil-app@0.1.0"
version="$(cargo pkgid --manifest-path "$root/Cargo.toml" -p moil-app | sed 's/.*[#@]//')"
# CFBundleVersion takes only numbers and dots.
bundle_version="${version%%-*}"

echo "Building moil-app and moil $version for ${arch}…"
cargo build --release --manifest-path "$root/Cargo.toml" -p moil-app -p moil-cli

echo "Fetching uv ${UV_VERSION}…"
tarball="uv-$arch-apple-darwin.tar.gz"
expected_var="UV_SHA256_$arch"
expected="${!expected_var}"
cache="$out/cache/uv-$UV_VERSION"
mkdir -p "$cache"
checksum() { shasum -a 256 "$1" | cut -d' ' -f1; }
if [ ! -f "$cache/$tarball" ] || [ "$(checksum "$cache/$tarball")" != "$expected" ]; then
    curl --fail --location --retry 3 --silent --show-error \
        --output "$cache/$tarball.part" \
        "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/$tarball"
    actual="$(checksum "$cache/$tarball.part")"
    if [ "$actual" != "$expected" ]; then
        rm -f "$cache/$tarball.part"
        echo "build.sh: $tarball has SHA-256 $actual, expected $expected" >&2
        exit 1
    fi
    mv "$cache/$tarball.part" "$cache/$tarball"
fi
rm -rf "$cache/uv-$arch-apple-darwin"
tar -xzf "$cache/$tarball" -C "$cache"

echo "Assembling ${app}…"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/moil-app" "$root/target/release/moil" "$app/Contents/MacOS/"
# The runtime's find_uv looks next to the executable.
cp "$cache/uv-$arch-apple-darwin/uv" "$app/Contents/MacOS/"
sed -e "s/@VERSION@/$version/" -e "s/@BUNDLE_VERSION@/$bundle_version/" \
    "$here/Info.plist" > "$app/Contents/Info.plist"
plutil -lint -s "$app/Contents/Info.plist"

iconset="$out/Moil.iconset"
rm -rf "$iconset"
swift "$here/icon.swift" "$iconset"
iconutil --convert icns --output "$app/Contents/Resources/Moil.icns" "$iconset"
rm -rf "$iconset"

echo "Signing (ad hoc)…"
# Nested code first, then the bundle, which seals the main executable.
for binary in uv moil; do
    codesign --force --sign - --timestamp=none "$app/Contents/MacOS/$binary"
done
codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict --verbose=1 "$app"

echo "Built $app"

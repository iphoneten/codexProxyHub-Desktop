#!/usr/bin/env bash
set -euo pipefail

APP_NAME="recodexProxyHub"
BIN_NAME="recodex-proxy-hub"
BUNDLE_ID="com.recodex.proxyhub"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
TARGET="${1:-$(rustc -vV | awk '/^host:/ { print $2 }')}"

RAW_VERSION="${RECODEX_VERSION:-}"
if [[ -z "$RAW_VERSION" && "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
  RAW_VERSION="${GITHUB_REF_NAME:-}"
fi
if [[ -z "$RAW_VERSION" ]]; then
  RAW_VERSION="$(git -C "$ROOT_DIR" describe --tags --exact-match HEAD 2>/dev/null || true)"
fi
if [[ -z "$RAW_VERSION" ]]; then
  RAW_VERSION="$(awk -F '"' '/^version = "/ { print $2; exit }' "$ROOT_DIR/Cargo.toml")"
fi

VERSION="${RAW_VERSION#v}"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: invalid release version: $RAW_VERSION" >&2
  echo "expected tag format: v1.2.3" >&2
  exit 1
fi
export RECODEX_VERSION="$VERSION"

case "$TARGET" in
  aarch64-apple-darwin)
    ARCH_NAME="arm64"
    ;;
  x86_64-apple-darwin)
    ARCH_NAME="x86_64"
    ;;
  *)
    echo "error: unsupported macOS target: $TARGET" >&2
    echo "supported: aarch64-apple-darwin, x86_64-apple-darwin" >&2
    exit 1
    ;;
esac

WORK_DIR="$DIST_DIR/build-macos-$ARCH_NAME"
APP_DIR="$WORK_DIR/$APP_NAME.app"
DMG_STAGE_DIR="$WORK_DIR/dmg-root"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
DMG_PATH="$DIST_DIR/$APP_NAME-macos-$ARCH_NAME.dmg"
APPICONSET_DIR="$ROOT_DIR/icon/AppIcons/Assets.xcassets/AppIcon.appiconset"
ICON_PNG="$ROOT_DIR/icon/icon_1024.png"
ICONSET_DIR="$WORK_DIR/$APP_NAME.iconset"
ICON_ICNS_NAME="AppIcon.icns"

command -v cargo >/dev/null 2>&1 || {
  echo "error: cargo not found. Install Rust first: https://rustup.rs/" >&2
  exit 1
}

command -v hdiutil >/dev/null 2>&1 || {
  echo "error: hdiutil not found. DMG packaging must run on macOS." >&2
  exit 1
}

if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "error: Rust target is not installed: $TARGET" >&2
  echo "install: rustup target add $TARGET" >&2
  exit 1
fi

if [[ -d "$APPICONSET_DIR" || -f "$ICON_PNG" ]]; then
  command -v iconutil >/dev/null 2>&1 || {
    echo "error: iconutil not found. macOS icon generation requires iconutil." >&2
    exit 1
  }
  if [[ ! -d "$APPICONSET_DIR" ]]; then
    command -v sips >/dev/null 2>&1 || {
      echo "error: sips not found. macOS icon generation requires sips." >&2
      exit 1
    }
  fi
fi

cd "$ROOT_DIR"
cargo build --release --target "$TARGET"

rm -rf "$WORK_DIR" "$DMG_PATH"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

cp "$ROOT_DIR/target/$TARGET/release/$BIN_NAME" "$MACOS_DIR/$APP_NAME"
chmod +x "$MACOS_DIR/$APP_NAME"

if [[ -f "$ROOT_DIR/config.yaml" ]]; then
  cp "$ROOT_DIR/config.yaml" "$RESOURCES_DIR/config.yaml"
elif [[ -f "$ROOT_DIR/config.example.yaml" ]]; then
  cp "$ROOT_DIR/config.example.yaml" "$RESOURCES_DIR/config.yaml"
fi

if [[ -d "$APPICONSET_DIR" || -f "$ICON_PNG" ]]; then
  rm -rf "$ICONSET_DIR"
  mkdir -p "$ICONSET_DIR"

  if [[ -d "$APPICONSET_DIR" ]]; then
    cp "$APPICONSET_DIR/16.png" "$ICONSET_DIR/icon_16x16.png"
    cp "$APPICONSET_DIR/32.png" "$ICONSET_DIR/icon_16x16@2x.png"
    cp "$APPICONSET_DIR/32.png" "$ICONSET_DIR/icon_32x32.png"
    cp "$APPICONSET_DIR/64.png" "$ICONSET_DIR/icon_32x32@2x.png"
    cp "$APPICONSET_DIR/128.png" "$ICONSET_DIR/icon_128x128.png"
    cp "$APPICONSET_DIR/256.png" "$ICONSET_DIR/icon_128x128@2x.png"
    cp "$APPICONSET_DIR/256.png" "$ICONSET_DIR/icon_256x256.png"
    cp "$APPICONSET_DIR/512.png" "$ICONSET_DIR/icon_256x256@2x.png"
    cp "$APPICONSET_DIR/512.png" "$ICONSET_DIR/icon_512x512.png"
    cp "$APPICONSET_DIR/1024.png" "$ICONSET_DIR/icon_512x512@2x.png"
  else
    sips -z 16 16 "$ICON_PNG" --out "$ICONSET_DIR/icon_16x16.png" >/dev/null
    sips -z 32 32 "$ICON_PNG" --out "$ICONSET_DIR/icon_16x16@2x.png" >/dev/null
    sips -z 32 32 "$ICON_PNG" --out "$ICONSET_DIR/icon_32x32.png" >/dev/null
    sips -z 64 64 "$ICON_PNG" --out "$ICONSET_DIR/icon_32x32@2x.png" >/dev/null
    sips -z 128 128 "$ICON_PNG" --out "$ICONSET_DIR/icon_128x128.png" >/dev/null
    sips -z 256 256 "$ICON_PNG" --out "$ICONSET_DIR/icon_128x128@2x.png" >/dev/null
    sips -z 256 256 "$ICON_PNG" --out "$ICONSET_DIR/icon_256x256.png" >/dev/null
    sips -z 512 512 "$ICON_PNG" --out "$ICONSET_DIR/icon_256x256@2x.png" >/dev/null
    sips -z 512 512 "$ICON_PNG" --out "$ICONSET_DIR/icon_512x512.png" >/dev/null
    sips -z 1024 1024 "$ICON_PNG" --out "$ICONSET_DIR/icon_512x512@2x.png" >/dev/null
  fi

  if ! iconutil --convert icns --output "$RESOURCES_DIR/$ICON_ICNS_NAME" "$ICONSET_DIR" 2>/dev/null; then
    echo "warning: iconutil failed, using built-in ICNS fallback." >&2
    command -v python3 >/dev/null 2>&1 || {
      echo "error: iconutil failed and python3 fallback is unavailable." >&2
      exit 1
    }
    python3 - "$ICONSET_DIR" "$RESOURCES_DIR/$ICON_ICNS_NAME" <<'PY'
import struct
import sys
from pathlib import Path

iconset = Path(sys.argv[1])
output = Path(sys.argv[2])
entries = [
    ("icp4", "icon_16x16.png"),
    ("icp5", "icon_32x32.png"),
    ("icp6", "icon_32x32@2x.png"),
    ("ic07", "icon_128x128.png"),
    ("ic08", "icon_256x256.png"),
    ("ic09", "icon_512x512.png"),
    ("ic10", "icon_512x512@2x.png"),
    ("ic11", "icon_16x16@2x.png"),
    ("ic12", "icon_32x32@2x.png"),
    ("ic13", "icon_128x128@2x.png"),
    ("ic14", "icon_256x256@2x.png"),
]
chunks = []
for icon_type, filename in entries:
    data = (iconset / filename).read_bytes()
    chunks.append(icon_type.encode("ascii") + struct.pack(">I", len(data) + 8) + data)

output.write_bytes(b"icns" + struct.pack(">I", 8 + sum(len(c) for c in chunks)) + b"".join(chunks))
PY
  fi
  rm -rf "$ICONSET_DIR"
else
  echo "warning: icon source not found: $APPICONSET_DIR or $ICON_PNG" >&2
fi

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>zh_CN</string>
  <key>CFBundleExecutable</key>
  <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleIconFile</key>
  <string>${ICON_ICNS_NAME%.icns}</string>
  <key>CFBundleIconName</key>
  <string>${ICON_ICNS_NAME%.icns}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

mkdir -p "$DMG_STAGE_DIR"
cp -R "$APP_DIR" "$DMG_STAGE_DIR/$APP_NAME.app"
ln -s /Applications "$DMG_STAGE_DIR/Applications"

hdiutil create \
  -volname "$APP_NAME" \
  -srcfolder "$DMG_STAGE_DIR" \
  -ov \
  -format UDZO \
  "$DMG_PATH"

echo "DMG created: $DMG_PATH"
echo "Version: $VERSION"

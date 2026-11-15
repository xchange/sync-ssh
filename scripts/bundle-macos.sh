#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script builds a macOS .app and must run on macOS." >&2
  exit 1
fi

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"
# Usage: scripts/bundle-macos.sh [destination-directory] [release|debug]
destination="${1:-$project_root/dist}"
build_profile="${2:-release}"
if [[ "$destination" != /* ]]; then destination="$project_root/$destination"; fi
case "$build_profile" in
  release) cargo build --locked --release -p sync-ssh ;;
  debug) cargo build --locked -p sync-ssh ;;
  *) echo "Profile must be release or debug." >&2; exit 1 ;;
esac

package_id="$(cargo pkgid --locked -p sync-ssh)"
app_version="${package_id##*[@#]}"
if [[ ! "$app_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Expected a numeric three-part application version, got: $app_version" >&2
  exit 1
fi

bundle_path="$destination/SyncSSH.app"
target_directory="${CARGO_TARGET_DIR:-$project_root/target}"
mkdir -p "$bundle_path/Contents/MacOS" "$bundle_path/Contents/Resources"
# Publish a new inode so an already running application can keep its mapped executable.
staged_binary="$(mktemp "$bundle_path/Contents/MacOS/.sync-ssh.XXXXXX")"
trap 'rm -f "$staged_binary"' EXIT
cp "$target_directory/$build_profile/sync-ssh" "$staged_binary"
chmod 755 "$staged_binary"
mv -f "$staged_binary" "$bundle_path/Contents/MacOS/sync-ssh"
trap - EXIT
cp "$project_root/crates/syncssh-app/assets/fonts/OFL.txt" "$bundle_path/Contents/Resources/Font-License.txt"
cp "$project_root/crates/syncssh-app/assets/fonts/UPSTREAM.md" "$bundle_path/Contents/Resources/Font-Upstream.md"
cp "$project_root/crates/syncssh-app/assets/icons/SyncSSH.icns" "$bundle_path/Contents/Resources/SyncSSH.icns"
cat > "$bundle_path/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>SyncSSH</string>
<key>CFBundleDisplayName</key><string>SyncSSH</string>
<key>CFBundleExecutable</key><string>sync-ssh</string>
<key>CFBundleIconFile</key><string>SyncSSH.icns</string>
<key>CFBundleIdentifier</key><string>dev.syncssh.SyncSSH</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$app_version</string>
<key>CFBundleVersion</key><string>$app_version</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict></plist>
PLIST
# Local ad-hoc signing; this is not Developer ID signing or notarization.
codesign --force --sign - "$bundle_path"
echo "Built: $bundle_path"

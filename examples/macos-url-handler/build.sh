#!/usr/bin/env bash
# Build Gpam Approve.app: a macOS https:// URL handler that routes GCP PAM
# approval links into a running gpam TUI via `gpam send`.
#
# After building, open ~/Applications/Gpam\ Approve.app once so macOS registers
# it as an https: handler, then right-click any PAM approval link and choose
# "Open With > Gpam Approve".
set -euo pipefail

cd "$(dirname "$0")"
OUT_DIR="${HOME}/Applications"
APP="${OUT_DIR}/Gpam Approve.app"

mkdir -p "$OUT_DIR"
rm -rf "$APP"
osacompile -o "$APP" gpam-approve.applescript

PLIST="${APP}/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Delete :CFBundleURLTypes" "$PLIST" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes array" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0 dict" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLName string com.github.gpam.approve" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes array" "$PLIST"
/usr/libexec/PlistBuddy -c "Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string https" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier com.github.gpam.approve" "$PLIST" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Add :CFBundleIdentifier string com.github.gpam.approve" "$PLIST"

# Editing Info.plist after osacompile invalidates the ad-hoc signature.
# Re-sign or macOS silently denies Automation permission prompts.
codesign --force --deep --sign - "$APP"

echo "built: $APP"

#!/bin/bash
set -euo pipefail

app="${1:?usage: sign-app.sh <app> [signing-identity]}"
identity="${2:--}"
repo="$(cd "$(dirname "$0")/.." && pwd)"

if [[ "$identity" == "-" ]]; then
  codesign --force --sign - "$app"
  codesign --verify --deep --strict --all-architectures "$app"
  echo "Built $app (ad-hoc signed; authentication sessions require Developer ID signing)"
  exit 0
fi

# Read the actual signer's team so certificate names and hashes both work.
codesign --force --sign "$identity" --timestamp=none "$app"
signature="$(codesign --display --verbose=2 "$app" 2>&1)"
team_id="$(sed -n 's/^TeamIdentifier=//p' <<<"$signature")"
if [[ -z "$team_id" || "$team_id" == "not set" ]]; then
  echo "sign-app: $identity has no TeamIdentifier; use a Developer ID Application identity" >&2
  exit 1
fi
bundle_id="$(plutil -extract CFBundleIdentifier raw -o - "$app/Contents/Info.plist")"
app_id="$team_id.$bundle_id"
entitlements="$(mktemp)"
trap 'rm -f "$entitlements"' EXIT
cp "$repo/Grinch.entitlements" "$entitlements"
/usr/libexec/PlistBuddy \
  -c "Add :com.apple.application-identifier string $app_id" "$entitlements"

codesign --force --options runtime --timestamp \
  --entitlements "$entitlements" --sign "$identity" "$app"
codesign --verify --deep --strict --all-architectures \
  --test-requirement="=anchor apple generic and \
entitlement[\"com.apple.application-identifier\"] = \"$app_id\"" "$app"
echo "Built $app (signed as $app_id)"

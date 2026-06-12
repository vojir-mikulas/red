#!/usr/bin/env bash
# Sign (hardened runtime), notarize, and staple build/Red.app.
#
# Local use — relies on your keychain identity and notary credentials:
#   SIGN_IDENTITY="Developer ID Application: Mikulas Vojir (ZGT84Z73N9)" \
#   NOTARY_KEY=~/private/AuthKey_XXXX.p8 NOTARY_KEY_ID=XXXX NOTARY_ISSUER=uuid \
#   ./scripts/sign-mac.sh
#
# Set SKIP_NOTARIZE=1 to only sign (useful before the API key exists).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/build/Red.app"
: "${SIGN_IDENTITY:?set SIGN_IDENTITY to your 'Developer ID Application: …' string}"

echo "▸ signing $APP"
codesign --force --timestamp --options runtime \
  --entitlements "$ROOT/crates/red/resources/red.entitlements" \
  --sign "$SIGN_IDENTITY" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

if [[ "${SKIP_NOTARIZE:-0}" == "1" ]]; then
  echo "▸ SKIP_NOTARIZE=1 — signed only (Gatekeeper will still warn until notarized)"
  exit 0
fi

: "${NOTARY_KEY:?set NOTARY_KEY to the AuthKey_*.p8 path}"
: "${NOTARY_KEY_ID:?set NOTARY_KEY_ID}"
: "${NOTARY_ISSUER:?set NOTARY_ISSUER}"

echo "▸ notarizing (this can take a few minutes)"
ditto -c -k --keepParent "$APP" "$ROOT/build/Red.zip"
xcrun notarytool submit "$ROOT/build/Red.zip" \
  --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" --wait

echo "▸ stapling"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl -a -vvv --type exec "$APP" || true
echo "▸ done — $APP is signed, notarized, stapled"

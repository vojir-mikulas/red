# macOS Release & Code Signing

> Production-grade workflow for shipping **Red** on macOS: build → bundle `.app`
> → sign with a Developer ID → notarize with Apple → staple → package a `.dmg`,
> all automated from a `v*` git tag in CI.

This is the canonical release guide. It assumes the repo state as of writing:
the binary target is **`Red`** (`crates/red/Cargo.toml`), all runtime assets
(fonts, icons, default settings) are **baked into the binary** via `rust-embed`,
and there is currently **no `.app` bundle**, so macOS derives the menu title from
the executable filename. Bundling is the first thing this guide adds.

## Why this matters

Without code signing + notarization, macOS Gatekeeper shows users a
"**Red** can't be opened because Apple cannot check it for malicious software"
dialog, and `quarantine`-flagged downloads are blocked outright. A notarized,
stapled `.app` inside a signed `.dmg` opens with a normal double-click on any
modern macOS. That is the bar for a public download.

We distribute **outside the Mac App Store** using a **Developer ID Application**
certificate. This needs the hardened runtime and notarization, but **not** the
App Sandbox, so the DB drivers' network access and Keychain use work without
sandbox entitlements.

---

## 0. Prerequisites (one-time, ~1 day incl. Apple processing)

| Item | How | Notes |
|------|-----|-------|
| **Apple Developer Program** | <https://developer.apple.com/programs/> | $99/yr. Individual is fine; the Team ID is what matters. |
| **Team ID** | Membership page → "Team ID" (10 chars, e.g. `A1B2C3D4E5`) | Goes into the bundle/notarization. |
| **Developer ID Application certificate** | See §1 | The signing identity for distributed apps. |
| **App Store Connect API key** | See §2 | Used by `notarytool` in CI; no Apple ID password needed there. |
| Xcode command-line tools | `xcode-select --install` | Provides `codesign`, `notarytool`, `stapler`, `iconutil`. |

> **Bundle identifier:** pick one now and never change it; it ties the
> notarization ticket, Keychain items, and `defaults` domain together. Use a
> reverse-DNS string you control, e.g. `dev.vojir.red` or `com.github.vojir-mikulas.red`.
> This guide uses **`dev.vojir.red`** as a placeholder; replace consistently.

---

## 1. Developer ID Application certificate

Create it once, then reuse it everywhere (local + CI).

1. In **Xcode → Settings → Accounts → Manage Certificates → + → Developer ID
   Application**, or via the [Developer portal](https://developer.apple.com/account/resources/certificates/list).
2. Verify it landed in your login keychain:
   ```sh
   security find-identity -v -p codesigning
   # → "Developer ID Application: Your Name (A1B2C3D4E5)"
   ```
3. **Export for CI** (so GitHub Actions can sign): Keychain Access → right-click
   the cert → **Export** → `.p12`, set a strong password. You'll base64 it into
   a GitHub secret (§7).
   ```sh
   base64 -i DeveloperID.p12 | pbcopy   # → paste into MACOS_CERT_P12 secret
   ```

---

## 2. App Store Connect API key (for notarization)

`notarytool` authenticates with an API key instead of an Apple ID + app-specific
password. Cleaner for CI and not tied to one person.

1. [App Store Connect → Users and Access → Integrations → App Store Connect API](https://appstoreconnect.apple.com/access/integrations/api)
   → generate a key with the **Developer** role (Account Holder must create it).
2. Download the `AuthKey_XXXXXXXXXX.p8` **once** (it's unrecoverable). Note:
   - **Key ID** (e.g. `XXXXXXXXXX`)
   - **Issuer ID** (UUID at the top of the page)
3. For CI, base64 the `.p8`:
   ```sh
   base64 -i AuthKey_XXXXXXXXXX.p8 | pbcopy   # → MACOS_NOTARY_KEY secret
   ```

---

## 3. Bundle the `.app`

A macOS `.app` is a directory with a fixed layout. Because all Red assets are
embedded in the binary, the bundle is minimal: binary + `Info.plist` + icon.

```
Red.app/
  Contents/
    Info.plist
    MacOS/
      Red            ← the cargo-built executable
    Resources/
      Red.icns       ← app icon
```

### 3a. Icon - generate `Red.icns`

We have `assets/red.svg` (the brand mark). Produce a 1024px master PNG, then an
`.iconset`, then the `.icns`. Add this as a committed helper at
`scripts/make-icns.sh`:

```sh
#!/usr/bin/env bash
# Generate Resources/Red.icns from a 1024x1024 PNG master.
# Requires: a PNG export of assets/red.svg at 1024px (see note below).
set -euo pipefail
SRC="${1:?usage: make-icns.sh path/to/icon-1024.png}"
OUT="${2:-build/Red.icns}"
ICONSET="$(mktemp -d)/Red.iconset"; mkdir -p "$ICONSET"

for size in 16 32 128 256 512; do
  sips -z $size $size       "$SRC" --out "$ICONSET/icon_${size}x${size}.png"      >/dev/null
  sips -z $((size*2)) $((size*2)) "$SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
mkdir -p "$(dirname "$OUT")"
iconutil -c icns "$ICONSET" -o "$OUT"
echo "wrote $OUT"
```

> SVG→PNG: `rsvg-convert -w 1024 -h 1024 assets/red.svg -o build/icon-1024.png`
> (`brew install librsvg`), or export once from a design tool and commit
> `assets/icon-1024.png` so CI needs no SVG toolchain. The brand mark is a mask
> tinted to the accent `#dc2626`; bake a solid background into the icon master
> so it reads on both Finder light/dark.

### 3b. `Info.plist`

Commit `crates/red/resources/Info.plist`. `__VERSION__` is substituted from the
git tag at build time (§5/§6).

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>Red</string>
  <key>CFBundleDisplayName</key>     <string>Red</string>
  <key>CFBundleIdentifier</key>      <string>dev.vojir.red</string>
  <key>CFBundleExecutable</key>      <string>Red</string>
  <key>CFBundleIconFile</key>        <string>Red</string>
  <key>CFBundleShortVersionString</key> <string>__VERSION__</string>
  <key>CFBundleVersion</key>         <string>__VERSION__</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>LSMinimumSystemVersion</key>  <string>11.0</string>
  <key>NSHighResolutionCapable</key> <true/>
  <key>NSHumanReadableCopyright</key> <string>© Mikuláš Vojíř. GPL-3.0-or-later.</string>
  <!-- App-managed titlebar; no document types. Red is a single-window utility. -->
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
</dict>
</plist>
```

### 3c. Bundle script

Commit `scripts/bundle-mac.sh`: it builds release, assembles the `.app`, and
substitutes the version. (Signing/notarization live in §4–6 and call into this.)

```sh
#!/usr/bin/env bash
set -euo pipefail
VERSION="${1:-$(git describe --tags --always | sed 's/^v//')}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/build/Red.app"
TARGET="${TARGET:-}"   # e.g. "aarch64-apple-darwin"; empty = host arch

echo "▸ cargo build --release"
cargo build -p red --release ${TARGET:+--target "$TARGET"}
BIN="$ROOT/target/${TARGET:+$TARGET/}release/Red"

echo "▸ assembling $APP (v$VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Red"
sed "s/__VERSION__/$VERSION/g" "$ROOT/crates/red/resources/Info.plist" \
  > "$APP/Contents/Info.plist"
cp "$ROOT/build/Red.icns" "$APP/Contents/Resources/Red.icns"
echo "▸ done: $APP"
```

> **Universal binary (recommended for distribution):** build both arches and
> `lipo` them so one download runs native on Apple Silicon and Intel:
> ```sh
> rustup target add aarch64-apple-darwin x86_64-apple-darwin
> cargo build -p red --release --target aarch64-apple-darwin
> cargo build -p red --release --target x86_64-apple-darwin
> lipo -create -output build/Red \
>   target/aarch64-apple-darwin/release/Red \
>   target/x86_64-apple-darwin/release/Red
> ```
> Then copy `build/Red` into the bundle instead of a single-arch binary.

---

## 4. Code signing (hardened runtime)

Notarization **requires** the hardened runtime (`--options runtime`). Sign
**inside-out**: any nested code first, then the outer `.app` last. Red has no
nested frameworks today, so it's a single `codesign` of the bundle.

Commit `crates/red/resources/red.entitlements`. Red is **not** sandboxed; this
file stays minimal. Only add an entitlement when something breaks under the
hardened runtime (it shouldn't: no JIT, no unsigned plugin loading).

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- Intentionally minimal. Developer ID + hardened runtime, no App Sandbox.
       DB network access and Keychain reads need no entitlement outside the
       sandbox. Add com.apple.security.cs.* only if a hardened-runtime crash
       proves it necessary. -->
</dict>
</plist>
```

Sign:

```sh
IDENTITY="Developer ID Application: Your Name (A1B2C3D4E5)"
codesign --force --timestamp --options runtime \
  --entitlements crates/red/resources/red.entitlements \
  --sign "$IDENTITY" build/Red.app

# verify
codesign --verify --deep --strict --verbose=2 build/Red.app
spctl -a -vvv --type exec build/Red.app   # "accepted" once notarized & stapled
```

> `--timestamp` (secure timestamp) is mandatory for notarization. `spctl` will
> say "rejected" until §5 staples the ticket; that's expected.

---

## 5. Notarize & staple

Submit the signed app to Apple, wait for the ticket, staple it so it verifies
offline.

```sh
# zip the .app for submission (notarytool takes a zip/dmg/pkg)
ditto -c -k --keepParent build/Red.app build/Red.zip

xcrun notarytool submit build/Red.zip \
  --key   AuthKey_XXXXXXXXXX.p8 \
  --key-id XXXXXXXXXX \
  --issuer 00000000-0000-0000-0000-000000000000 \
  --wait

# on success, staple the ticket onto the .app
xcrun stapler staple build/Red.app
xcrun stapler validate build/Red.app
```

If notarization fails, fetch the log; it pinpoints the offending binary/flag:

```sh
xcrun notarytool log <submission-id> --key … --key-id … --issuer …
```

---

## 6. Package the `.dmg`

Ship a drag-to-Applications `.dmg`. `create-dmg` (`brew install create-dmg`)
gives a polished window; a plain `hdiutil` image works too.

```sh
create-dmg \
  --volname "Red __VERSION__" \
  --app-drop-link 480 200 \
  --icon "Red.app" 160 200 \
  --window-size 640 400 \
  "build/Red-__VERSION__.dmg" "build/Red.app"

# sign + notarize the DMG itself too (so the download, not just the app, passes)
codesign --force --timestamp --sign "$IDENTITY" build/Red-__VERSION__.dmg
xcrun notarytool submit build/Red-__VERSION__.dmg --key … --key-id … --issuer … --wait
xcrun stapler staple build/Red-__VERSION__.dmg
```

> Notarize the **app first** (§5), then the **dmg**. Stapling both means the
> download verifies even fully offline.

---

## 7. CI automation - release on tag

Add `.github/workflows/release.yml`. It fires on a `v*` tag (cut a release with
`git tag vX.Y.Z && git push origin vX.Y.Z`), builds a universal `.app`, signs,
notarizes, staples, packages, and uploads to a GitHub Release. The existing `ci.yml` (fmt/clippy/test) stays the gate for `main`/PRs;
this is additive.

### Required repository secrets

Settings → Secrets and variables → Actions:

| Secret | Source |
|--------|--------|
| `MACOS_CERT_P12` | base64 of the Developer ID `.p12` (§1) |
| `MACOS_CERT_PASSWORD` | the `.p12` export password |
| `MACOS_SIGN_IDENTITY` | `Developer ID Application: Your Name (A1B2C3D4E5)` |
| `MACOS_NOTARY_KEY` | base64 of `AuthKey_XXXXXXXXXX.p8` (§2) |
| `MACOS_NOTARY_KEY_ID` | the Key ID |
| `MACOS_NOTARY_ISSUER` | the Issuer ID |
| `KEYCHAIN_PASSWORD` | any random string (temp CI keychain password) |

### Workflow

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write   # create the GitHub Release + upload assets

jobs:
  macos:
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin,x86_64-apple-darwin

      - uses: Swatinem/rust-cache@v2

      - name: Import signing certificate
        env:
          CERT_P12: ${{ secrets.MACOS_CERT_P12 }}
          CERT_PASSWORD: ${{ secrets.MACOS_CERT_PASSWORD }}
          KEYCHAIN_PASSWORD: ${{ secrets.KEYCHAIN_PASSWORD }}
        run: |
          KEYCHAIN=$RUNNER_TEMP/build.keychain
          echo "$CERT_P12" | base64 --decode > $RUNNER_TEMP/cert.p12
          security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
          security set-keychain-settings -lut 21600 "$KEYCHAIN"
          security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
          security import $RUNNER_TEMP/cert.p12 -k "$KEYCHAIN" \
            -P "$CERT_PASSWORD" -T /usr/bin/codesign
          security set-key-partition-list -S apple-tool:,apple: \
            -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
          security list-keychains -d user -s "$KEYCHAIN" login.keychain

      - name: Build universal .app
        run: |
          VERSION="${GITHUB_REF_NAME#v}"
          rsvg-convert -w 1024 -h 1024 assets/red.svg -o build/icon-1024.png || true
          ./scripts/make-icns.sh assets/icon-1024.png build/Red.icns
          cargo build -p red --release --target aarch64-apple-darwin
          cargo build -p red --release --target x86_64-apple-darwin
          mkdir -p build/Red.app/Contents/MacOS build/Red.app/Contents/Resources
          lipo -create -output build/Red.app/Contents/MacOS/Red \
            target/aarch64-apple-darwin/release/Red \
            target/x86_64-apple-darwin/release/Red
          sed "s/__VERSION__/$VERSION/g" crates/red/resources/Info.plist \
            > build/Red.app/Contents/Info.plist
          cp build/Red.icns build/Red.app/Contents/Resources/Red.icns

      - name: Sign
        env:
          IDENTITY: ${{ secrets.MACOS_SIGN_IDENTITY }}
        run: |
          codesign --force --timestamp --options runtime \
            --entitlements crates/red/resources/red.entitlements \
            --sign "$IDENTITY" build/Red.app
          codesign --verify --deep --strict --verbose=2 build/Red.app

      - name: Notarize & staple
        env:
          KEY: ${{ secrets.MACOS_NOTARY_KEY }}
          KEY_ID: ${{ secrets.MACOS_NOTARY_KEY_ID }}
          ISSUER: ${{ secrets.MACOS_NOTARY_ISSUER }}
        run: |
          echo "$KEY" | base64 --decode > $RUNNER_TEMP/key.p8
          ditto -c -k --keepParent build/Red.app build/Red.zip
          xcrun notarytool submit build/Red.zip \
            --key $RUNNER_TEMP/key.p8 --key-id "$KEY_ID" --issuer "$ISSUER" --wait
          xcrun stapler staple build/Red.app

      - name: Package .dmg
        env:
          IDENTITY: ${{ secrets.MACOS_SIGN_IDENTITY }}
          KEY_ID: ${{ secrets.MACOS_NOTARY_KEY_ID }}
          ISSUER: ${{ secrets.MACOS_NOTARY_ISSUER }}
        run: |
          VERSION="${GITHUB_REF_NAME#v}"
          brew install create-dmg
          create-dmg --volname "Red $VERSION" --app-drop-link 480 200 \
            --icon "Red.app" 160 200 --window-size 640 400 \
            "build/Red-$VERSION.dmg" "build/Red.app"
          codesign --force --timestamp --sign "$IDENTITY" "build/Red-$VERSION.dmg"
          xcrun notarytool submit "build/Red-$VERSION.dmg" \
            --key $RUNNER_TEMP/key.p8 --key-id "$KEY_ID" --issuer "$ISSUER" --wait
          xcrun stapler staple "build/Red-$VERSION.dmg"

      - name: Publish GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          files: build/Red-*.dmg
          generate_release_notes: true
          fail_on_unmatched_files: true
```

---

## 8. Release checklist

Cutting `vX.Y.Z`:

1. [ ] `just check` green (fmt · clippy · test).
2. [ ] `CHANGELOG.md`: move _Unreleased_ → `## [X.Y.Z]`, dated.
3. [ ] Bump `version` in root `Cargo.toml` `[workspace.package]`; commit `Cargo.lock`.
4. [ ] `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. [ ] Watch the **Release** workflow; confirm notarization succeeded (not just "submitted").
6. [ ] Download the `.dmg` on a **clean Mac** (or fresh VM), double-click, confirm
       it opens with **no Gatekeeper prompt**. `spctl -a -vvv /Applications/Red.app`
       → `accepted`.
7. [ ] Edit the GitHub Release notes; mark latest.

### Local dry-run (before trusting CI)

Run the whole chain on your own machine once with `scripts/bundle-mac.sh` + the
§4–6 commands. The first notarization is where mistakes surface (missing
`--timestamp`, wrong identity, hardened-runtime entitlement gaps).

---

## 9. Suggested rollout order

Land it in small, verifiable steps rather than one big CI commit:

1. **Bundle + icon, unsigned.** Add `Info.plist`, `red.entitlements`,
   `scripts/make-icns.sh`, `scripts/bundle-mac.sh`; add a `just bundle` recipe.
   Confirm `build/Red.app` launches locally. *(No Apple account needed.)*
2. **Enroll** in the Apple Developer Program; create the Developer ID cert (§1)
   and API key (§2).
3. **Sign + notarize locally** (§4–6) and verify on a clean Mac. This proves the
   identity, entitlements, and bundle id before any CI.
4. **Automate** (§7): add secrets + `release.yml`, push a throwaway `v0.0.1-rc1`
   tag to a test, then cut the real tag.

> **Cost/effort:** ~$99/yr + ~half a day of setup. Steps 1 and 3 are where the
> real debugging is; CI is mechanical once a local notarization works.

## Future / out of scope

- **Auto-update** (Sparkle or a custom check against GitHub Releases); a
  notarized appcast feed builds on this pipeline.
- **Homebrew Cask**: once `.dmg` releases are stable, a `cask` makes
  `brew install --cask red` trivial and is low-maintenance.
- **Windows / Linux packaging**: separate guides; this one is macOS-only.

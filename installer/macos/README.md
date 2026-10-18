# Native macOS application and drag-to-Applications installer

The Apple Silicon package includes the native manager, CLI engine, a small
native launch adapter, Vulkan Loader and KosmicKrisp. It requires macOS 26+
for the current Venus memory mappings/driver. No SDK environment or global
Vulkan installation is required at runtime.

Build release binaries outside the checkout using the dev-environment rules:

```sh
MACOSX_DEPLOYMENT_TARGET=15.0 cargo build --release -p entangled -p entangled-manager --locked
```

Render the procedural Entangled icon and installer art with Python/Pillow,
then convert the iconset with Apple's `iconutil`:

```sh
python3 scripts/macos-installer-art.py /path/to/art
iconutil -c icns /path/to/art/Entangled.iconset -o /path/to/art/Entangled.icns
```

Package with a Developer ID Application identity already present in the
keychain. The notices directory must contain the project license, runtime
licenses and provenance; public releases additionally need the complete
Rust/SDK third-party notice audit and release gates.

```sh
python3 scripts/package-macos.py \
  --bin-dir "$CARGO_TARGET_DIR/release" --sdk "$VULKAN_SDK" \
  --notices /path/to/notices --icon /path/to/art/Entangled.icns \
  --identity 'Developer ID Application: NAME (TEAM)' \
  --output '/path/to/output/Entangled Desktop.app'
```

The adapter selects the bundle's own loader and ICD before execing the
engine. All native code is signed with the same identity and timestamp.
Hardened runtime and library validation stay enabled; only the engine receives
the hypervisor and DYLD-environment entitlements needed by this launch path.
Packaging verifies ARM64 inputs, refuses external non-system dependencies,
records input digests and verifies the final bundle signature. Existing output
apps are never replaced.

The installer uses build-time MIT-licensed `ds_store` and `mac_alias` tools
to write a Retina background and icon layout, and Apple's `hdiutil` to create
the compressed image. It contains the app and a link to `/Applications`.

```sh
python3 scripts/make-macos-dmg.py \
  --app '/path/to/output/Entangled Desktop.app' \
  --background /path/to/art/background.png \
  --identity 'Developer ID Application: NAME (TEAM)' \
  --output /path/to/output/Entangled-macos-arm64.dmg
```

Notarization is asynchronous and stores its request ID. The optional credentials
file accepts ZenX's literal `ZENX_NOTARY_KEY_ID`, `ZENX_NOTARY_ISSUER` and
`ZENX_NOTARY_KEY` assignments without executing shell code or displaying secrets.
Without it, Apple's existing `zenx-notary` keychain profile is used.

```sh
python3 scripts/notarize-macos.py submit /path/to/installer.dmg \
  --request /path/to/notary-request.json --credentials-env /path/to/notary.env
python3 scripts/notarize-macos.py status /path/to/installer.dmg \
  --request /path/to/notary-request.json --credentials-env /path/to/notary.env
# Only after the recorded request is Accepted:
python3 scripts/notarize-macos.py staple /path/to/installer.dmg
spctl --assess --type open --context context:primary-signature /path/to/installer.dmg
```

Inspect the actual mounted Finder window before delivery, and test the packaged
engine's `doctor` and real guest Vulkan acceptance with SDK environment variables
removed. The native manager and Ubuntu ARM64 backend remain experimental;
Automatic verified Ubuntu ARM64 installation is implemented; lifecycle,
long-run stability and fast image sharing still have the gaps recorded in
ADR-0007. The signed installer does not imply those gaps
are complete. No release is uploaded or published by these scripts.


## Automatic GitHub releases

`.github/workflows/release.yml` builds the macOS app on an ARM64 `macos-26`
runner for every push to `main`. Its version comes from the same workflow run
as the Linux engine and Windows installer. The pinned Vulkan SDK archive is
checked against `vulkan-sdk.json` before its installer executes; only its core
package is installed in a task-owned directory, without global system changes.

The repository requires the same six encrypted Actions secrets as ZenX:

- `APPLE_DEVELOPER_ID_P12_BASE64`: Developer ID Application certificate and
  private key, encoded as a password-protected PKCS#12 file in base64.
- `APPLE_DEVELOPER_ID_P12_PASSWORD`: that file's password.
- `APPLE_TEAM_ID`: the Developer ID team's identifier.
- `APPLE_API_KEY_P8_BASE64`: base64 App Store Connect API private key.
- `APPLE_API_KEY_ID` and `APPLE_API_ISSUER_ID`: its key and issuer identifiers.

The signing identity is imported into a temporary keychain. The private API
key is written with mode 0600 and removed even after a failed step. Key material
never enters the source tree, release assets, diagnostics or build cache.
The runtime license texts, SDK provenance and generated Rust dependency report
are bundled in the app. Build output is cached before task-owned files are
removed.

Publication waits for all platform jobs. Apple must return `Accepted`; the
workflow then staples and validates the ticket, checks signatures, Gatekeeper
and DMG integrity, and calculates SHA-256 **after** stapling. A failed, timed-out
or missing notarization prevents publication of the whole release. The public
assets are `Entangled-<version>-macos-arm64.dmg` and `SHA256SUMS-macos.txt`.
The request/report is retained as a separate Actions diagnostic artifact.

For credential rotation, update the six repository secrets using GitHub's
Actions settings or `gh secret set`, passing secret values through stdin rather
than command-line arguments. Existing ZenX repository secrets cannot be read
back or copied through the GitHub API; configure Entangled from the original
local certificate and API key instead.

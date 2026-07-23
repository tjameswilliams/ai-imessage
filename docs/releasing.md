# Releasing

The full cycle for cutting a signed, notarized release. Prerequisites
(already set up): a `Developer ID Application` certificate in the
keychain, and notarization credentials stored as a notarytool keychain
profile (`xcrun notarytool store-credentials <profile>` with an
app-specific password from account.apple.com).

## 1. Version and tag

```bash
# bump version = "X.Y.Z" in Cargo.toml, then:
cargo build --release            # refreshes Cargo.lock
git commit -am "vX.Y.Z"
git tag vX.Y.Z
git push && git push origin vX.Y.Z
gh release create vX.Y.Z --title "vX.Y.Z" --notes "…changelog…"
```

## 2. Build, sign, notarize, upload

```bash
./scripts/release.sh X.Y.Z --notary-profile <profile>
```

This builds `--release --locked`, signs with the Developer ID identity
(hardened runtime + timestamp), verifies, submits to Apple's notary
service and waits for `Accepted`, tarballs the binary, uploads it to the
GitHub release, and prints the formula stanza with the sha256.

## 3. Update the tap

In `tjameswilliams/homebrew-tap` → `Formula/ai-imessage.rb`:

- `version "X.Y.Z"`
- `on_arm`: the release-artifact URL and sha256 printed by the script
- `on_intel`: the source tarball URL for the new tag and its sha256
  (`curl -sL <tag-tarball-url> | shasum -a 256`)

Commit and push the tap.

## 4. Verify

```bash
brew update && brew upgrade ai-imessage
codesign --display --verbose=2 /opt/homebrew/bin/ai-imessage   # Developer ID authority
ai-imessage doctor
ai-imessage service status    # sync still green — the FDA grant must survive
```

## Why signing matters here

macOS TCC anchors the Full Disk Access grant to the binary's
code-signing requirement (team identity + identifier), not its bytes.
Signed releases therefore keep the grant across upgrades; an unsigned or
ad-hoc build would silently invalidate it, and users would see
permission errors in `service status` until they re-toggled the grant.
Never ship an unsigned release for this reason.

# How MonHop is signed

Every release is signed three ways. Release signing happens only in GitHub Actions, so a lost Mac never stops a release, as long as the master copies below are safe.

| What | Signed with | What it gives people |
|---|---|---|
| Mac app and DMG | Developer ID Application certificate for Gozzi LLC, team `2CU4CSTZWR` | macOS opens MonHop without the "unidentified developer" block, and permissions survive updates because the app's identity stays the same |
| Notarization of the app and DMG | App Store Connect team API key, Developer role | Apple's malware-scan approval is attached ("stapled") to both files, so the check also works offline |
| Update files (`.app.tar.gz`, `-setup.exe`) | MonHop updater key, minisign key ID `73B03A45DB934B17` | Installed copies only install updates signed by this key, whose public half is built into the app |
| Windows installer | not code-signed | Windows may show SmartScreen or Defender warnings |
| Local development builds | "MonHop Development Signing", a self-signed identity in this Mac's keychain (`scripts/macos-signing.sh`) | Local builds keep their macOS permissions between rebuilds; never shipped |

## The one key that cannot be replaced

If the **updater key** is lost, no installed copy of MonHop can ever update itself again. Every user would have to download a new version by hand. GitHub never shows a stored secret again, so the copy in GitHub cannot be retrieved. Keep the Bitwarden copy current, and never generate a replacement "just for CI".

The other keys can be replaced in minutes (see [Replacing one secret](#replacing-one-secret)).

## Where every secret lives

**Master copies: Bitwarden**, in one Secure Note named `MonHop release signing`. Everything is stored as text, because file attachments need Bitwarden Premium:

| Where in the note | Contents | Needed? |
|---|---|---|
| Hidden field `updater private key` | the one line in `~/.tauri/monhop-updater.key` (copy it with `pbcopy < ~/.tauri/monhop-updater.key`) | **Required.** This is the only copy outside GitHub. |
| Notes | the Developer ID `.p12` as base64 (`base64 -i Certificates.p12 \| pbcopy`), exported together with Apple's G2 intermediate | Recommended. Without it, a lost Mac means creating a new certificate. |
| Hidden field `p12 password` | the password chosen when exporting the `.p12` | With the `.p12` |

Not stored, on purpose:
- **The `.p8` notarization key.** A lost Mac means revoking it and generating a new one anyway, which takes two minutes.
- **The updater public key.** It is in `tauri.conf.json`.
- **The Key ID and Issuer ID.** Both are shown on Apple's Team Keys page.

After pasting a key into Bitwarden, clear the clipboard with `pbcopy < /dev/null`.

The **Issuer ID** identifies the Gozzi LLC team to Apple. A request needs the `.p8`, its Key ID and the Issuer ID together. Find it at appstoreconnect.apple.com → Users and Access → Integrations tab → App Store Connect API → Team Keys tab. It is the value labeled "Issuer ID" above the keys table: 36 characters in five dash-separated groups, such as `57246542-96fe-1a63-e053-0824d011072a`. It is the same for every key on the team.

To make a new notarization key on the same page, click **+** next to Active. Name it `MonHop notarization`, set Access to **Developer** and click **Generate**. Then click **Download API Key** right away, because Apple allows only one download.

**Working copies: the `release` environment on GitHub** (Settings → Environments → release). It accepts only the `main` branch and `v*` tags, so a workflow pushed on any other branch cannot read these secrets. Pull requests never receive them. No signing secret may exist at repository level, where every branch could read it.

| Secret | Contents |
|---|---|
| `APPLE_CERTIFICATE` | the `.p12`, base64 |
| `APPLE_CERTIFICATE_PASSWORD` | its export password |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Gozzi LLC (2CU4CSTZWR)` |
| `APPLE_API_KEY` | the API Key ID |
| `APPLE_API_ISSUER` | the Issuer ID |
| `APPLE_API_KEY_P8` | the `.p8` file's contents |
| `TAURI_SIGNING_PRIVATE_KEY` | the updater private key file's contents |

`scripts/store-release-secrets.sh` uploads all seven. It checks every input before uploading anything:
- the updater key signs a test file that verifies against the public key built into MonHop
- the `.p12` holds a Developer ID certificate with its matching private key
- the `.p8` is a readable private key

**On the maintainer's Mac**, which is FileVault-encrypted:
- `~/.tauri/monhop-updater.key` stays on disk for local builds (`scripts/build-macos-app.sh`)
- the Developer ID identity lives in the login keychain
- the `.p12` and `.p8` files are deleted once the upload succeeds and the `.p12` is in Bitwarden

## What a release build does

`build.yml` runs one job per platform in the `release` environment. Each job does this:

1. Installs tauri-cli from source with no build cache. Caches written by ordinary `main` CI runs must never reach the job that holds the keys.
2. On macOS only, writes the `.p8` to the runner's temporary folder. The Apple credentials never reach the Windows job.
3. Runs Tauri, which:
   - signs the app with the Developer ID certificate and hardened runtime
   - notarizes and staples the app
   - signs the DMG
   - signs each update file with the updater key
4. Checks every update signature against the public key in `tauri.conf.json` (`scripts/verify_updater_signature.py`). A wrong updater key fails the build here instead of shipping updates nobody can install.
5. Notarizes and staples the DMG, which Tauri does not do. Then fails unless Gatekeeper accepts both the app and the DMG.
6. Deletes the `.p8`.

`scripts/tauri_ci.py` blocks three mistakes:
- partially configured Apple settings
- notarization without a certificate
- the retired Apple ID login (`APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`), which Tauri would otherwise prefer over the API key, and whose app-specific password also opens the Apple account's iCloud data

## If the Mac is lost or stolen

Do these in order, the same day:

1. **GitHub first.** The Mac's `gh` login can push to `main` and push release tags. At github.com → Settings → Sessions, sign out the lost Mac. Under Settings → Applications, revoke GitHub CLI and any tokens it used. Without push access, a stolen updater key cannot reach users: installed copies only download from this repository's releases.
2. **Revoke the API key.** In App Store Connect → Users and Access → Integrations → Team Keys, revoke the notarization key. Then make a new one. Revoking is free and harmless.
3. **Leave the Developer ID certificate alone** unless the private key may really be exposed, for example if the Mac was unlocked or FileVault was off. Revoking it can stop already-shipped copies from opening. If it must go, contact Apple Developer Support before revoking so shipped builds are considered, then follow [Replacing one secret](#replacing-one-secret).
4. **Leave the updater key alone** unless it may really be exposed. Rotating it needs a transition release (see below).
5. Set up the new Mac and re-upload with the new API key.

## Setting up a new Mac

1. Install the tools:
   - Xcode and Homebrew
   - `brew install gh openssl@3`, then `gh auth login`
   - Bitwarden
   - Rust through rustup, then `cargo install tauri-cli --version 2.11.4 --locked`
2. Restore the updater key: copy the `updater private key` field from Bitwarden, then run `mkdir -p ~/.tauri && pbpaste > ~/.tauri/monhop-updater.key && chmod 600 ~/.tauri/monhop-updater.key && pbcopy < /dev/null`.
3. Create a fresh local development identity with `bash scripts/macos-signing.sh setup` (see README). Reinstall MonHop and re-grant its permissions once.
4. Only if GitHub's copies need replacing:
   - restore the `.p12`: copy the note's base64 text from Bitwarden, then run `pbpaste | base64 -d > Certificates.p12 && pbcopy < /dev/null`
   - generate and download a new `.p8` on the Team Keys page (revoke the old key there too)
   - run `scripts/store-release-secrets.sh <file.p12> <AuthKey_KEYID.p8>`
   - delete both files
5. Prove the setup without publishing anything:
   - run `gh workflow run build.yml --ref main`
   - check the downloads with the commands in [Checking a build by hand](#checking-a-build-by-hand)

The Developer ID certificate only needs to be in this Mac's keychain to export a new `.p12`. Releases never sign on a Mac. If `security find-identity -v -p codesigning` lists it as invalid, add Apple's intermediate: download `https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer` and run `security add-certificates -k ~/Library/Keychains/login.keychain-db DeveloperIDG2CA.cer`.

## Replacing one secret

Upload replacements with `scripts/store-release-secrets.sh`, update Bitwarden, then run a `build` workflow from `main`.

| Situation | What to do | Effect on users |
|---|---|---|
| Developer ID certificate expiring (current one: 2027-11-12) | developer.apple.com → Certificates → **+** → Developer ID Application → G2 Sub-CA, using a request made in Keychain Access → Certificate Assistant. Install it in the **login** keychain. Export it with the G2 intermediate as a `.p12`. | None. The app's signing requirement names the bundle ID, an Apple-issued Developer ID and team `2CU4CSTZWR`, not one certificate, so permissions survive. Notarized builds keep working after their certificate expires. |
| `.p12` password forgotten | Export a new `.p12` from the keychain that still holds the key. Otherwise create a new certificate as above. | None |
| API key lost or leaked | On the Team Keys page, revoke it, then generate and download a new one as described above | None |
| Updater key leaked | Release a version that builds in a new public key, signed one last time with the old key. Then switch the secret and Bitwarden to the new key. | One extra update |
| Updater key lost | Generate a new key, put its public half in `tauri.conf.json`, and release. Installed copies cannot verify it, so tell users to download the new version by hand once. | Everyone reinstalls manually |

## When a build fails

| Log says | Meaning | What to do |
|---|---|---|
| `failed to notarize app: ... The Internet connection appears to be offline` | GitHub's Mac runner lost its network while waiting for Apple. `notarytool` stops on the first network error. | Re-run the failed jobs |
| `Notarizing .../MonHop.app` with no output for a long time | Apple is still reviewing the upload. A new team's first submissions took about three hours in October 2026; later ones take minutes. | Wait. The job gives up after GitHub's 6-hour limit, and Apple keeps reviewing either way. |
| `Configure TAURI_SIGNING_PRIVATE_KEY in the release environment` | The run cannot see the environment's secrets | Check that the run started from `main` or a `v*` tag and that the secret exists |
| `... is not allowed to deploy to release due to environment protection rules` | The run started from another branch | Expected. Only `main` and `v*` tags may sign. |
| `signed by key ..., expected 73B03A45DB934B17` | `TAURI_SIGNING_PRIVATE_KEY` holds the wrong key | Re-run `scripts/store-release-secrets.sh` with the key from Bitwarden |
| `tauri CI configuration error` | Apple secrets are missing or retired | The message names them. Run `scripts/store-release-secrets.sh` again. |

## Checking a build by hand

Download a `build` run's artifacts (`gh run download <run-id>`), then check:

```sh
codesign -dvvv MonHop.app 2>&1 | grep -E 'Authority=Developer ID|TeamIdentifier|Timestamp'
spctl --assess --type execute --verbose MonHop.app        # accepted, source=Notarized Developer ID
xcrun stapler validate MonHop.app                         # The validate action worked!
spctl --assess --type open --context context:primary-signature --verbose MonHop_*.dmg
xcrun stapler validate MonHop_*.dmg
python3 scripts/verify_updater_signature.py *.sig         # verified with key 73B03A45DB934B17
```

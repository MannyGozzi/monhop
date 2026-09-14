# Releasing MonHop

## Keep the changelog current

Every change a person would notice goes into the Unreleased section of `CHANGELOG.md` as one plain line, under Added, Changed, Fixed or Security. Write it for someone who has never seen the code: what they can now do, what stopped going wrong, no file names and no internal terms. The Unreleased section becomes the release notes, linked from the download page, and the release script refuses to run while it is empty.

## Cut the release

From a clean `main` that matches origin, run the script with the size of the change:

```
python3 scripts/release.py patch      # or minor, major, or an explicit 1.2.0
```

Add `--dry-run` first if you want to read the plan before anything moves. The plan names the old and new version, the tag, and every file the release touches. The script checks the preconditions (on `main`, nothing uncommitted, level with origin, an Unreleased section with entries, the tag still free, the version moving forwards), then bumps the workspace version and the exact pins between the workspace crates, refreshes the lockfile offline, regenerates and rechecks the dependency reports, moves Unreleased under the new version and date in the changelog, commits as `Release vX.Y.Z` and tags `vX.Y.Z`.

Nothing leaves the machine until you push. Pass `--push` to the script, or run the two commands it prints afterwards. Pushing the tag is what starts the release.

## What the tag does

The tag first checks that its version matches the workspace and has release notes, and that the repository is public. A private repository cannot serve the website's unauthenticated download links. The workflow never changes repository visibility itself.

It then runs the full gate set and builds Apple-silicon, Intel-Mac, and Windows installers in parallel. Each builder uploads a separate workflow artifact and has no release-write permission. You can also run the `build` workflow manually to test packaging without creating a release.

One final job downloads all three artifacts. `scripts/assemble_release.py` requires both DMGs, both Mac updater archives, the Windows installer, and the updater signature files. It gives the Mac archives distinct architecture-specific names and writes `latest.json` once, with version-tagged download URLs. Missing or empty files stop publication. Signature files come from the Tauri build; the assembly step checks their presence, not their cryptographic validity.

The publisher creates one draft, uploads the complete asset set, compares the uploaded names and sizes, then publishes it as latest. A failed upload leaves the draft unpublished. Re-running a failed publish may resume that draft, but never overwrite an already published release. Releases are serialized so separate tags cannot publish concurrently. If only the post-publication public-access check fails, investigate the published release rather than re-running the upload against it.

The final check requests the latest-release API and every download without authentication. The website discovers the versioned DMG and EXE names from that public API, so no website rebuild is needed for a new release. Also test an installed copy's update check and installation before calling the release verified end to end.

## Secrets the workflow needs

`TAURI_SIGNING_PRIVATE_KEY` must contain the existing updater private key, not its path. Its public half is built into the app. Do not generate a replacement key for CI: existing installations would reject updates signed with it. Builds check for the key before installing the toolchain and fail instead of producing unsigned updater artifacts.

The maintainer's local key is at `~/.tauri/monhop-updater.key`. With explicit authorization to send it to this repository's GitHub Actions secrets, configure it without printing it:

```sh
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo MannyGozzi/monhop < "$HOME/.tauri/monhop-updater.key"
```

`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` is optional for the current unencrypted key. An absent secret resolves to an empty value. For an encrypted key, set the matching password separately and never commit either value. Keep a separate encrypted backup in a password manager: GitHub can use a saved secret but does not provide its value for recovery.

Apple signing and notarization are optional: set `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` to sign and notarize the Mac builds. Until they exist the Mac app is ad hoc signed and macOS asks the person installing it to allow the app by hand. The CI wrapper omits absent Apple credentials instead of passing empty certificate values to Tauri; partially configured credentials fail with a setup error.

## Rolling back

Delete the release on GitHub, then delete the tag locally and on origin (`git tag -d vX.Y.Z` and `git push origin :refs/tags/vX.Y.Z`). The updater only ever looks at the newest published release, so the version before it becomes current again for everyone who has not updated yet.

That does not reach anyone who already installed the bad build. For them the fix goes forwards: land the repair on `main` and cut the next patch release.

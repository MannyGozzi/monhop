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

The tag runs the full gate set (the same one `main` runs: the Rust gates on macOS and Windows, the site build, the script tests, the audits and the dependency report check), then builds three signed bundles (Apple silicon, Intel, and the Windows installer) and uploads them into a single draft release with the changelog notes as the body.

A last job holds the door. It refuses to publish unless the tag matches the workspace version, the tag has exactly one release, and `latest.json` carries a signed bundle for all three platforms. Only then does the release go public and become the latest one. That order matters: the updater reads only the newest published release, so a half-built one can never reach anybody.

On the release page, check that the notes read like the changelog you wrote, that all three installers are there with a signature file each and `latest.json` beside them, and that the release is published, marked latest, and not a prerelease. Then let an installed copy check for the update and take it.

## Secrets the workflow needs

`TAURI_SIGNING_PRIVATE_KEY` is the maintainer's updater signing key; its public half is built into the app, and a build without the key fails rather than shipping an unsigned update. `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` must exist but is empty, because the key is stored without one.

Apple signing and notarization are optional: set `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` to sign and notarize the Mac builds. Until they exist the Mac app is ad hoc signed and macOS asks the person installing it to allow the app by hand.

## Rolling back

Delete the release on GitHub, then delete the tag locally and on origin (`git tag -d vX.Y.Z` and `git push origin :refs/tags/vX.Y.Z`). The updater only ever looks at the newest published release, so the version before it becomes current again for everyone who has not updated yet.

That does not reach anyone who already installed the bad build. For them the fix goes forwards: land the repair on `main` and cut the next patch release.

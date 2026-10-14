# Releasing MonHop

## Keep the changelog current

Write release notes once in `CHANGELOG.md`. Keep notable, user-visible changes under Unreleased, grouped by Added, Changed, Fixed or Security. Open a substantial release with a one-sentence summary, then use short bullets that explain what a person can do or what stopped going wrong. Skip commit messages, internal refactors and empty categories. Include compatibility changes, required actions and installation limitations. Aim for a one-minute read.

The release script moves Unreleased into a dated version and the workflow publishes that exact section as the GitHub release body. The website reads published notes from GitHub, not a second content file. Do not hand-maintain website copies or use GitHub-generated commit lists. Update the changelog before publication so the repository, release and website agree.

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

Builds sign with secrets stored in the repository's `release` environment, which admits only `main` and `v*` tags. [SIGNING.md](SIGNING.md) explains each key, where its master copy lives, how to upload it with `scripts/store-release-secrets.sh`, how to check a build by hand, and how to recover from a lost Mac. Builds stop before installing the toolchain when the updater key is missing, and fail when an updater signature does not match the key built into the app. The updater key is the one secret that can never be regenerated.

## Rolling back

Delete the release on GitHub, then delete the tag locally and on origin (`git tag -d vX.Y.Z` and `git push origin :refs/tags/vX.Y.Z`). The updater only ever looks at the newest published release, so the version before it becomes current again for everyone who has not updated yet.

That does not reach anyone who already installed the bad build. For them the fix goes forwards: land the repair on `main` and cut the next patch release.

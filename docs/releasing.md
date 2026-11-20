# Releasing

spanbolt versions follow semver; the version lives once, in the workspace `Cargo.toml`. Only
repository admins can create `v*` and `shim-*` tags (a tag ruleset).

## Cut vX.Y.Z

1. Open a PR titled `chore(release): vX.Y.Z` that sets `version = "X.Y.Z"` under
   `[workspace.package]` in `Cargo.toml` and updates `Cargo.lock` (`cargo update --workspace`).
   Merge it once CI is green.
2. Tag the merge commit on main and push the tag:

   ```sh
   git fetch origin main
   git tag -a vX.Y.Z -m "spanbolt X.Y.Z" origin/main
   git push origin vX.Y.Z
   ```

3. The tag starts two workflows:
   - `release.yml` checks the tag matches `Cargo.toml` and is on main, then creates the GitHub
     release with notes generated from the merged PRs, grouped by label (`.github/release.yml`).
     Label PRs (`breaking`, `enhancement`, `bug`, ...) before releasing so the notes sort well.
   - `image.yml` publishes `ghcr.io/strise/spanbolt:X.Y.Z` and `:X.Y`.
4. Check the release page and the image, and edit the notes if they need a summary.

A tag with a suffix (`v0.2.0-rc.1`) makes a pre-release. Don't move or delete a published tag: cut
a new patch release instead.

## GoogleSQL shim

The prebuilt shim has its own tags, `shim-<GOOGLESQL_VERSION>-s<SHIM_VERSION>`, published by
`shim.yml` and then pinned in `native/googlesql-shim.env`. See `docs/building.md`.

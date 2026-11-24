# Releasing

spanbolt versions follow semver; the version lives once, in the workspace `Cargo.toml`. Only
repository admins can create `v*` and `shim-*` tags (a tag ruleset).

## Cut vX.Y.Z

1. Open a PR titled `chore(release): vX.Y.Z` that sets `version = "X.Y.Z"` under
   `[workspace.package]` in `Cargo.toml`, `version = "=X.Y.Z"` on each internal crate in
   `[workspace.dependencies]` (the crates are released together), and updates `Cargo.lock`
   (`cargo update --workspace`). Merge it once CI is green.
2. Tag the merge commit on main and push the tag:

   ```sh
   git fetch origin main
   git tag -a vX.Y.Z -m "spanbolt X.Y.Z" origin/main
   git push origin vX.Y.Z
   ```

3. The tag starts three workflows:
   - `release.yml` checks the tag matches `Cargo.toml` and is on main, then creates the GitHub
     release with notes generated from the merged PRs, grouped by label (`.github/release.yml`).
     Label PRs (`breaking`, `enhancement`, `bug`, ...) before releasing so the notes sort well.
   - `image.yml` publishes `ghcr.io/strise/spanbolt:X.Y.Z` and `:X.Y`.
   - `crates.yml` publishes the crates to crates.io (below).
4. Check the release page, the image and crates.io, and edit the notes if they need a summary.

A tag with a suffix (`v0.2.0-rc.1`) makes a pre-release. Don't move or delete a published tag: cut
a new patch release instead.

## GoogleSQL shim

The prebuilt shim has its own tags, `shim-<GOOGLESQL_VERSION>-s<SHIM_VERSION>`, published by
`shim.yml` and then pinned in `native/googlesql-shim.env`. See `docs/building.md`.

## crates.io

`cargo install spanbolt` builds the binary from crates.io. Published, in dependency order:

```
googlesql-sys  spanbolt-ddl  spanbolt-proto  spanbolt-storage  spanbolt-txn
spanbolt-sql   spanbolt-server  spanbolt
```

`spanbolt-conformance` is `publish = false`. All crates share the workspace version. `crates.yml`
publishes them on a `v*` tag with trusted publishing (no token in the repository), skipping any
already on crates.io at that version, so rerunning a failed run is safe. Run it from the Actions tab
on a branch (workflow_dispatch) for a dry run: it packages and builds every crate and publishes
nothing. Locally, `cargo package --workspace --exclude spanbolt-conformance` does the same.

Trusted publishing can't create a crate, so the first release goes by hand, once:

1. Log in with a crates.io token that can publish new crates: `cargo login`.
2. From a clean checkout of the release tag, check, then publish in the order above:

   ```sh
   cargo package --workspace --locked --exclude spanbolt-conformance
   for c in googlesql-sys spanbolt-ddl spanbolt-proto spanbolt-storage spanbolt-txn \
            spanbolt-sql spanbolt-server spanbolt; do
     cargo publish --locked --no-verify -p "$c" || break
   done
   ```

   (`cargo publish --workspace --locked` does the same in one command, but can't resume halfway.)
3. On crates.io, for each crate: Settings → Trusted Publishing → add GitHub, repository
   `strise/spanbolt`, workflow `crates.yml`, environment `crates-io`. Add co-owners
   (`cargo owner --add`) as needed.
4. Optionally restrict the `crates-io` environment (Settings → Environments) to `v*` tags.

After that, tags publish on their own. A new crate added to the workspace needs the same first
manual publish and trusted-publisher setup, and its name in `CRATES` in `crates.yml`.

docs.rs builds the libraries' docs. It has no network, so `googlesql-sys` builds there without the
shim and with a stub for the resolved-AST types; `spanbolt-sql`, which uses them, doesn't build on
docs.rs, and its `documentation` link points at `DESIGN.md` instead.

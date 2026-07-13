# Releasing

v0.1.0 shipped 2026-07-13: crates.io (`doover`, `doover-core`), the
`caydenchik/doover` Homebrew tap, and GitHub release binaries for
macOS (arm64/x86_64) and Linux (x86_64/arm64).

## Cutting a new release

1. Bump `version` in the workspace `Cargo.toml` (both the package version
   and the `doover-core` workspace dependency), run `make test`, commit.
2. Publish the crates, library first:

   ```console
   $ cargo publish -p doover-core
   $ cargo publish -p doover
   ```

3. Tag and push; the Release workflow builds all four platforms and
   publishes the GitHub Release with checksums:

   ```console
   $ git tag vX.Y.Z && git push origin vX.Y.Z
   ```

4. Update the tap: edit `Formula/doover.rb` in
   [homebrew-doover](https://github.com/CaydenChik/homebrew-doover) with the
   new version and the four sha256 values from the release's `SHA256SUMS`.
5. Sanity-check one install path (`cargo install doover --force` or
   `brew upgrade doover`) and run `doover doctor`.

## Standing decisions

- crates.io publishing is manual on purpose; a stray tag only ever builds
  binaries, it can't ship a crate.
- macOS notarization is deferred: brew/cargo installs don't carry the
  browser quarantine attribute, so Gatekeeper doesn't block them. Revisit
  if distribution widens beyond brew/cargo/release tarballs.
- CI runs `cargo audit` on every push; release builds are `--locked`
  against the committed `Cargo.lock`.

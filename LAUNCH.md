# Launch checklist

Everything below is ready to run; steps marked **(you)** need credentials
only the account owner has. Order matters.

## 1. Flip the repo public **(you)**

- GitHub → Settings → change visibility to public.
- Re-add the CI badge to the top of `README.md` (it renders once public):

  ```markdown
  [![CI](https://github.com/CaydenChik/doover/actions/workflows/ci.yml/badge.svg)](https://github.com/CaydenChik/doover/actions)
  ```

## 2. Publish the crates **(you, one-time token setup)**

```console
$ cargo login            # paste a crates.io token with publish scope
$ cargo publish -p doover-core
$ cargo publish -p doover        # after doover-core is live (a minute or two)
```

(`cargo package -p doover` can only verify once `doover-core` is on
crates.io; that's expected, not a breakage.)

Then update the README install section to the simpler:

```console
$ cargo install doover
```

## 3. Tag the release

```console
$ git tag v0.1.0
$ git push origin v0.1.0
```

The `Release` workflow builds macOS (arm64/x86_64) and Linux (x86_64/arm64)
binaries, checksums them, and publishes a GitHub Release with notes.

## 4. Stand up the Homebrew tap

```console
$ gh repo create CaydenChik/homebrew-doover --public --clone
$ mkdir -p homebrew-doover/Formula
$ cp packaging/homebrew/doover.rb homebrew-doover/Formula/
# fill each sha256 from the release's SHA256SUMS asset, then:
$ cd homebrew-doover && git add -A && git commit -m "doover 0.1.0" && git push
```

Verify on a clean machine: `brew tap caydenchik/doover && brew install doover
&& doover doctor`. Then add the brew instructions to the README.

## 5. Verify the install paths

On a machine that has never seen doover:

- `cargo install doover` → `doover init` → `doover doctor` → run an agent
  through a delete → `doover undo`.
- Same via the brew tap.
- Download a release tarball directly and check `shasum -a 256 -c` against
  `SHA256SUMS`.

## Decisions made (revisit post-launch if needed)

- **macOS notarization: deferred.** Homebrew- and cargo-installed binaries
  don't carry the browser quarantine attribute, so Gatekeeper doesn't block
  them; notarization matters only for binaries downloaded through a browser.
  Revisit if distribution moves beyond brew/cargo/release-tarballs, or sign
  with a Developer ID as polish. Requires an Apple Developer account.
- **crates.io publishing stays manual** rather than tag-triggered, so a bad
  tag can never ship a crate; the release workflow only builds binaries.
- **Supply chain:** CI now runs `cargo audit` (RustSec advisories) on every
  push; `Cargo.lock` is committed and release builds use `--locked`.

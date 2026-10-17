# Releases

openporch releases are built by [`dist`](https://github.com/axodotdev/cargo-dist)
0.32.0 from `dist-workspace.toml`, through the generated
`.github/workflows/release.yml`. Pushing a version tag such as `v0.1.0` builds
and publishes a GitHub release. Nothing is published to crates.io.

## What a release contains

| Archive | Binary | Platforms |
| --- | --- | --- |
| `openporch-server-<target>.tar.xz` | `openporch`, the server | Linux (x86_64 and aarch64, glibc), macOS (x86_64 and aarch64) |
| `openporch-cli-<target>.tar.xz` or `.zip` | `porch`, the command line | Linux (x86_64 and aarch64 glibc, x86_64 musl), macOS (x86_64 and aarch64), Windows (x86_64) |

The server is not built for Windows: its drivers manage OpenTofu processes with
Unix signals. Each archive holds the binary, `LICENSE` and `README.md`.

Beside the archives, a release has:

- a `.sha256` file per archive, and `sha256.sum` listing every archive and the
  source tarball;
- `source.tar.gz`, the tagged source;
- installer scripts: `openporch-server-installer.sh`,
  `openporch-cli-installer.sh` and `openporch-cli-installer.ps1`, which
  download the archive for the machine they run on into `~/.cargo/bin`;
- a [GitHub build provenance attestation](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations)
  for each archive, signed by the release workflow.

The OpenTofu provider is not part of a release yet; build it from `provider/`
as [Getting started](getting-started.md) describes.

## Verifying a download

Check an archive against the release's checksums, then check that the release
workflow in this repository built it:

```sh
sha256sum --ignore-missing -c sha256.sum
gh attestation verify openporch-server-x86_64-unknown-linux-gnu.tar.xz --repo krisbrud/openporch
```

On macOS, use `shasum -a 256 --ignore-missing -c sha256.sum`.

## Building from source

Every release build uses the toolchain pinned in `rust-toolchain.toml` and the
dependencies locked in `Cargo.lock`: before building, the workflow runs
`cargo fetch --locked`, which fails if `Cargo.lock` does not match the
manifests. To build the same binaries from a checkout of the tag:

```sh
git checkout v0.1.0
cargo build --locked --profile dist -p openporch-server -p openporch-cli
```

The binaries are `target/dist/openporch` and `target/dist/porch`; the release
workflow passes `--target`, so its builds land in `target/<target>/dist/`.
The `dist` profile is the release profile with thin link-time optimization.

## Cutting a release

1. Set `version` in the workspace `Cargo.toml` to the new version, run
   `cargo check` so `Cargo.lock` follows, and merge the change.
2. Check the plan: `dist plan` lists every archive and installer for the
   version. The `release plan` CI job runs the same check on every pull
   request.
3. Push a tag naming the same version, with a `v` prefix:
   `git tag v0.1.0 && git push origin v0.1.0`. A tag whose version does not
   match `Cargo.toml` fails the workflow before anything is built. A version
   with a prerelease suffix, such as `0.2.0-rc.1` tagged `v0.2.0-rc.1`, is
   published as a prerelease.

After changing `dist-workspace.toml`, `.github/build-setup.yml` or a crate's
`[package.metadata.dist]`, run `dist generate` and commit the regenerated
workflow; the `release plan` job fails until they match.

# Building and releasing

## Building from source

Every input a build uses is locked in the repository:

| Input | Locked by |
| --- | --- |
| Rust toolchain | `rust-toolchain.toml` (rustup installs that version) |
| Rust dependencies | `Cargo.lock`, used with `--locked` |
| Go toolchain | the `go` line of `provider/go.mod` |
| Go dependencies | `provider/go.sum`, used with `-mod=readonly` |

From a clean checkout:

```sh
cargo build --release --locked -p openporch-server -p openporch-cli
(cd provider && GOFLAGS=-mod=readonly go build -trimpath -o terraform-provider-openporch .)
```

`scripts/release-archive.sh OUT_DIR` does the same for the host and packages
the result as the release does:

- `openporch-VERSION-TARGET.tar.gz` holds `openporch`, `porch`, `LICENSE` and
  `README.md`;
- `terraform-provider-openporch_VERSION_GOOS_GOARCH.zip` holds the provider.
  Put it in a filesystem mirror under
  `registry.opentofu.org/openporch/openporch/`, as
  [getting started](getting-started.md#the-opentofu-provider) describes.

## What a release proves

The [release workflow](../.github/workflows/release.yml) runs on a `v*` tag:

1. It checks that the tag names the workspace version, that `Cargo.lock` and
   `go.sum` are complete and current, and that the tests pass from the locked
   inputs. The tests include the schema fixtures, the published schemas and
   the documented examples. `cargo deny` checks licenses and advisories.
2. It builds Linux x86_64, Linux aarch64 and macOS arm64 archives from that
   commit.
3. It writes `SHA256SUMS` for every archive, and a GitHub build provenance
   attestation that ties those checksums to the tag's commit and the workflow
   run.
4. It drafts a GitHub release with the archives and `SHA256SUMS`.

Pull requests that change the release build run steps 1 to 3 without
attesting or drafting.

What is reproducible: the source inputs. A build from the tagged commit
resolves exactly the locked toolchains and dependencies. The archives are not
claimed to be byte-for-byte reproducible, and that has not been verified. The
build strips local paths and timestamps it controls (`--remap-path-prefix`,
`-trimpath`, `gzip -n`, `zip -X`), but the linker, archive metadata and
platform toolchains are not pinned to deterministic output.

## Verifying a release

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify openporch-0.1.0-x86_64-unknown-linux-gnu.tar.gz \
  --repo krisbrud/openporch
```

The attestation names the commit and workflow run that built the archive.

## Release checklist

For the maintainer cutting a release:

1. On `main`, CI is green, including the OpenTofu driver and provider jobs.
2. Run what CI does not: `just e2e-aws` against the sandbox account
   ([e2e](e2e.md)) and `just bench` ([benchmark](benchmark.md)). Record the
   results in those pages.
   The [acceptance matrix](acceptance.md) has no gaps left, and every
   scenario it marks manual has passed for this release.
3. Review the dependency changes since the last release: `cargo deny check`
   passes, and every new license is in `deny.toml` for a reason.
4. Update [compatibility](compatibility.md): the contract versions, schema
   version, fixtures, tested matrix and anything a release changes for
   clients. When a contract breaks, write the migration guide for the
   release notes.
5. When a migration was added, check that its fixture exists in
   `crates/server/fixtures/schema` and that [operations](operations.md#upgrades-and-the-schema-version)
   lists the new pair.
6. Raise `version` in the workspace `Cargo.toml` and commit it with
   `Cargo.lock`.
7. Tag that commit `vX.Y.Z` and push the tag.
8. When the workflow has finished, download the draft's archives, check
   `SHA256SUMS` and the attestation as above, and run the
   [quickstart](quickstart.md) with the downloaded `openporch` and `porch`.
9. Write the release notes: changes, compatibility notes, migration steps,
   then publish the draft.

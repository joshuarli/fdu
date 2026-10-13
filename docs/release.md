# Releases

The manual [release workflow](../.github/workflows/release.yml) builds macOS
arm64 and Linux x86_64/arm64 musl artifacts. It publishes a
`release-<sha>` prerelease containing dynamically linked and static Linux
binaries.

The pinned nightly in [`rust-toolchain.toml`](../rust-toolchain.toml) and
`rust-src` are needed for release builds. Common Makefile targets are:

For a normal host build, use `cargo build --locked --release`. On an
Apple-silicon Mac, the release build and local install targets are:

```sh
make release TARGET=aarch64-apple-darwin
make install TARGET=aarch64-apple-darwin
```

`make release` builds a static Linux musl binary, or a macOS binary, using the
repository's release flags. `make release-dynamic` builds a dynamically linked
Linux musl binary. Linux release targets are built in the workflow's Docker
image, which provides the musl linker wrapper. It uses
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, verifies the
dynamic build with `make verify-release-dynamic`, then builds and verifies the
static build with `make release` and `make verify-release`. `make install`
copies the local release binary to `~/usr/bin/fdu` and ad-hoc signs it on
macOS. `make lint` runs Clippy with automatic fixes and may modify source
files.

Build a summary-only binary without terminal or deletion support with:

```sh
cargo build --locked --release --no-default-features
target/release/fdu --summary .
```

Linux release builds run in the repository's Docker image; see
[`Dockerfile`](../Dockerfile). The GitHub Actions workflow is started with
`workflow_dispatch`.

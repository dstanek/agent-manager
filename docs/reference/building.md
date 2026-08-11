# Building from Source

`am` is a single Rust binary with no non-Rust build dependencies, so it builds
cleanly on every supported platform with a stock toolchain.

## Developing in a dev container

The repo ships a [`.devcontainer/`](https://github.com/dstanek/agent-manager/tree/main/.devcontainer)
with everything the project's own checks need: the Rust toolchain with clippy and
rustfmt, `jj`, `tmux`, the docs toolchain, Node with `@devcontainers/cli`, and
`gh`. Open it in any editor that supports dev containers, or build it directly:

```sh
devcontainer build --workspace-folder .
```

`am` uses it for its own sessions automatically — the project dogfooding the
feature. `container.mode` defaults to `"auto"`, so `am start <slug>` in this repo
builds `.devcontainer/` and runs the session in it with no configuration at all.
Set `mode = "image"` in `.am/config.toml` to opt out.

Two things are deliberately **not** in the image. There is no container runtime
inside it, so `am start` itself has to be run on the host — the test suite mocks
Podman and Docker via `AM_PODMAN_BIN`/`AM_DOCKER_BIN` and needs neither. And `jj`
is pinned by `ARG JJ_VERSION` in the Dockerfile rather than tracking latest, so a
new jj release cannot change what CI-equivalent local runs are testing against.
Renovate proposes that bump as a pull request — see
[Dependency Updates](dependency-updates.md).

!!! note "Rebuild after changing the docs requirements"
    `am`'s config hash covers `devcontainer.json` and the Dockerfile, but not
    other files in the build context. `requirements-docs.txt` is one of those, so
    run `am start <slug> --rebuild` after changing it.

## Prerequisites

Building outside a dev container needs:

- [Rust](https://rustup.rs) 1.70 or later (edition 2021)
- A C linker, which every supported platform already provides:
  - **Linux** — the system linker from `gcc`/`binutils` (installed on virtually
    all distributions; `build-essential` on Debian/Ubuntu if missing)
  - **macOS** — the Xcode Command Line Tools (`xcode-select --install`)
  - **Windows** — the MSVC build tools (installed with Visual Studio or the
    standalone Build Tools package)

No system libraries, `pkg-config` entries, or headers are required — the crate
graph is pure Rust.

## Build

```sh
cargo build --release
```

The optimized binary is written to `target/release/am` (`am.exe` on Windows).
Verify it:

```sh
./target/release/am --version
```

## Supported targets

| Target triple                | Platform            | How it's verified                     |
|------------------------------|---------------------|---------------------------------------|
| `x86_64-unknown-linux-gnu`   | Linux x86_64        | Release CI matrix + local build       |
| `aarch64-unknown-linux-gnu`  | Linux ARM64         | Release CI matrix                      |
| `x86_64-apple-darwin`        | macOS Intel         | Release CI matrix                      |
| `aarch64-apple-darwin`       | macOS Apple Silicon | Release CI matrix                      |
| `x86_64-pc-windows-msvc`     | Windows x86_64      | Release CI matrix (experimental)       |

`cargo build --release` has been confirmed to produce a working binary on
`x86_64-unknown-linux-gnu` (`am --version` reports the crate version and all
subcommands are present).

## CI coverage

Two workflows exercise the build:

- **`.github/workflows/ci.yml`** — on pull requests to `main` that touch `src/`,
  `tests/`, `Cargo.toml`, `Cargo.lock`, `.cargo/`, or the workflow itself, runs
  `cargo clippy --all-targets -- -D warnings`, `cargo build`, and `cargo test` on
  `ubuntu-latest` (which build-checks Linux), plus a `cross-build` job that runs
  `cargo build --target <triple>` (build-only, no packaging) for
  `aarch64-apple-darwin` and `x86_64-pc-windows-msvc`. This catches
  macOS/Windows-specific compile breakage on the PR that introduces it rather
  than only at release time; the remaining targets are exercised at tag time.
- **`.github/workflows/release.yml`** — on every `v*` tag, runs
  `cargo build --release --target <triple>` across the full platform matrix
  above (Linux via `ubuntu-latest`/`ubuntu-24.04-arm`, macOS via
  `macos-latest`, Windows via `windows-latest`), then packages the artifacts
  and publishes them to the GitHub release.

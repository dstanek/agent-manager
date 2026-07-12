# Building from Source

`am` is a single Rust binary with no non-Rust build dependencies, so it builds
cleanly on every supported platform with a stock toolchain.

## Prerequisites

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

- **`.github/workflows/ci.yml`** — on every push and pull request to `main`,
  runs `cargo clippy -- -D warnings`, `cargo build`, and `cargo test` on
  `ubuntu-latest`.
- **`.github/workflows/release.yml`** — on every `v*` tag, runs
  `cargo build --release --target <triple>` across the full platform matrix
  above (Linux via `ubuntu-latest`/`ubuntu-24.04-arm`, macOS via
  `macos-latest`, Windows via `windows-latest`), packages the artifacts, and
  publishes them to the GitHub release.

Because macOS and Windows binaries are cross-checked only when a release tag is
pushed, a compile error specific to those platforms would not surface during a
normal PR. If that becomes a concern, add the release build matrix to `ci.yml`
as a build-only job (no packaging) so per-platform breakage is caught earlier —
at the cost of extra macOS/Windows runner minutes on every PR.

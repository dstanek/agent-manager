# CLAUDE.md

## Project Overview

`am` (Agent Manager) is a Rust CLI tool that creates isolated environments for coding agents (Claude Code, GitHub Copilot, Gemini, Codex, Aider, etc.). Each session gets its own git worktree or jj workspace, a dedicated tmux window with split panes, and optional containerization via Podman or Docker.

## Commands

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo test               # Run all tests (run after every change)
cargo clippy --all-targets -- -D warnings  # Lint, including tests (run after every change)
cargo run -- <command>   # Run (e.g., cargo run -- start my-feature)
make build-claude        # Build Claude Code Docker image
make build-copilot       # Build Copilot Docker image
```

## Testing

- `tempfile` crate for isolated test directories
- Mock tmux via `AM_TMUX_BIN`; mock container runtimes via `AM_PODMAN_BIN`/`AM_DOCKER_BIN`; mock the Dev Containers CLI via `AM_DEVCONTAINER_BIN`; point the Feature cache at a scratch directory via `AM_FEATURE_CACHE`
- Tests run single-threaded: `.cargo/config.toml` sets `RUST_TEST_THREADS = "1"`. A test that writes a mock script and then execs it fails with `ETXTBSY` if any other thread forks while the file is still open for writing, because the child inherits the write descriptor. Per-module mutexes cannot prevent this — the contended resource is the process-wide fd table — so the whole binary is serialized instead. Do not remove this without moving the exec-mock tests to their own test target.
- Test git fixtures commit with `--no-verify`: a developer's global `init.templatedir` can install a `commit-msg` hook into every `git init`
- Tests mutating env vars use a mutex to serialize execution
- The `cucumber` crate's `{string}` capture does not unescape *any* backslash sequence — not `\"`, not `\x1b`, not `\n` inside a `Then`/`And` assertion (as opposed to a `Given .. containing "..."` step, whose handler unescapes manually before writing a file — see e.g. `given_project_config`). A step written as `does not contain "agent = \"codex\""` or `does not contain "\x1b[2m"` compares against the literal backslash characters, which real output never contains, so it passes whether or not the thing it's meant to catch is present — a silently vacuous assertion (confirmed twice: once for `\"`, once for `\x1b` in `init.feature`, which stayed green through injected dimming it was written to catch). Fixes differ by what's being escaped: a double quote in the *expected* text — use single-quoted Gherkin strings (`does not contain 'agent = "codex"'`); an ANSI escape byte — compare the real `\u{1b}` byte in a dedicated Rust step function instead of through `{string}` (see `then_output_contains_dimmed_line`/`then_output_contains_plain_line`/`then_output_contains_note_line` in `tests/cucumber.rs`, and force color on for the scenario via `Given I have set env "NO_COLOR" to ""` + `And I have set env "CLICOLOR_FORCE" to "1"`, since the harness's default `NO_COLOR=1` never emits color to check in the first place).
- The pty-based interactive test harness (`run_am_pty` in `tests/cucumber.rs`) gives `am` a real pty via `script`; a scripted input line missing its trailing `\n` leaves the child blocked forever in canonical-mode `read()` rather than failing fast.
- Some Feature paths cannot be tested against the public registries at all — nothing published
  there declares `dependsOn`, and none of them is private. `scripts/test-registry.sh up` stands
  up what those tests need: a local `registry:2` with `tests/fixtures/registry/*` published to
  it, plus a second one behind htpasswd basic auth that the runtime is logged in to. Both must
  answer to `localhost`, because the reference CLI speaks plain HTTP only to that name and TLS
  to everything else; inside a dev container using docker-outside-of-docker the registry lives
  on the *host*, so the script forwards `localhost:<port>` with `socat`. Two traps it encodes:
  a backgrounded forwarder holding stdout makes the script never return, and a `-v` source path
  is resolved against the **host's** filesystem, so the htpasswd file is `cp`ed in rather than
  mounted (a bind mount silently creates a directory there and every request 400s).
- `scripts/test-live-session.sh` runs one session for real — tmux server, compose project up,
  `am destroy` — because the ordinary suite mocks the runtime, tmux, and the agent, and that
  combination is where compose bugs live. It runs inside `am`'s own dev container only because
  the checkout is mounted at its host path; the scratch repo therefore lives under `target/`, the
  session gets a scratch `HOME` under it, and assertions read *container* state rather than the
  bind-mounted worktree, which a sibling container cannot write to under a rootless runtime's
  user namespace.
- The tarball Feature fixture is committed and served over GitHub's HTTPS rather than locally:
  `ureq` verifies against a bundled root store, so no locally issued certificate can be trusted,
  and `am` accepts no other scheme. See `tests/fixtures/tarball/README.md`.
- The `devcontainer.metadata` label is the entire contract between the two image builders, so the `#[ignore]`d differential tests in `src/devcontainer/native/mod.rs` are what keep `am`'s own builder honest: they build the same config both ways and compare the label. Run them with `cargo test --bin am -- --ignored` (needs Docker, network, and the reference CLI on `PATH`). Set `AM_TEST_NO_CACHE=1` when the result matters — the runtime's layer cache survives `builder prune -af` and will finish a "passing" run in a couple of seconds without building anything. For anything about **install order**, reach for `devcontainer features resolve-dependencies` instead: it prints the CLI's resolved order without building, so the check costs a few manifest GETs rather than minutes.
- `run_am`/`run_am_with_input`/`run_am_pty` all set `NO_COLOR=1` on the child unconditionally, since `color::enabled` honours a developer's ambient `CLICOLOR_FORCE=1` even for a piped stdout. Without it, a `contains`/`does not contain` assertion on a severity-prefixed line (e.g. `Note: ...`) passes in a plain environment but fails the moment a developer has `CLICOLOR_FORCE=1` set — CI stays green, so it looks like *their* change broke something.

**After every code change:** run `cargo test` and `cargo clippy --all-targets -- -D warnings`. Fix any failures before proceeding. `--all-targets` matters — without it clippy skips test code, which is what CI lints.

## Path Handling Strategy

Prefer `Path`/`PathBuf`/`OsStr` over `String` — convert to `String` only at boundaries (command args, container mounts).

- `fn foo(path: &Path)` — not `fn foo(path: &str)`
- `.display()` for error messages and logging
- `.to_string_lossy()` inline at call sites, not into intermediate variables

## Version Control

Use `jj` commands (not `git`). Commits use **Conventional Commits**: `type(scope): description`.

Use `jj commit -m "..."` (not `jj describe`) to leave the working copy clean. Append this footer to every commit message:

```
---
Co-Piloted-By: am via Claude Code
```

For other trailer types (autonomous, review), see `docs/reference/commit-trailers.md`.

## Key Reference Files

- `specs/` — per-feature specs; each large feature gets one. Start here for design intent
- `BACKLOG.md` — feature status and what is left to do
- `docs/reference/configuration.md` — configuration reference
- `docs/reference/commands.md` — current behaviour of every command
- `PLAN.md` — the one-time bootstrap plan, kept as history. Not maintained; do not trust it for current behaviour

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
- Mock tmux via `AM_TMUX_BIN`; mock container runtimes via `AM_PODMAN_BIN`/`AM_DOCKER_BIN`; mock the Dev Containers CLI via `AM_DEVCONTAINER_BIN`
- Tests run single-threaded: `.cargo/config.toml` sets `RUST_TEST_THREADS = "1"`. A test that writes a mock script and then execs it fails with `ETXTBSY` if any other thread forks while the file is still open for writing, because the child inherits the write descriptor. Per-module mutexes cannot prevent this — the contended resource is the process-wide fd table — so the whole binary is serialized instead. Do not remove this without moving the exec-mock tests to their own test target.
- Test git fixtures commit with `--no-verify`: a developer's global `init.templatedir` can install a `commit-msg` hook into every `git init`
- Tests mutating env vars use a mutex to serialize execution
- The `cucumber` crate's `{string}` capture does not unescape `\"`, so a step written as `does not contain "agent = \"codex\""` compares against text containing literal backslashes and is always true — a silently vacuous assertion. Use single-quoted Gherkin strings when the expected text itself contains double quotes: `does not contain 'agent = "codex"'`.
- The pty-based interactive test harness (`run_am_pty` in `tests/cucumber.rs`) gives `am` a real pty via `script`; a scripted input line missing its trailing `\n` leaves the child blocked forever in canonical-mode `read()` rather than failing fast.

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

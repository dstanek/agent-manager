# Backlog

Outstanding work for `am`. Items are grouped by theme and roughly ordered by priority.

---

## Agent Integrations

### Feature 7: Codex Integration
> Spec: [`specs/codex-integration.md`](specs/codex-integration.md)

Env-var-only auth preset for OpenAI Codex — no filesystem mount needed.

**Fully implemented.** `KnownAgent::Codex` is accepted, `validate_agent_credentials` checks `OPENAI_API_KEY` is set (fails early with a clear message if not), `resolve_agent_auth_mount` returns an empty vec (no filesystem mount needed), and `agent_extra_env` injects `OPENAI_API_KEY` into the container.

- [x] Design: no mount; auth via `OPENAI_API_KEY` env var pass-through; `validate_agent("codex")` must pass
- [x] Implementation: `codex` branch in `resolve_agent_auth_mount()` returns empty vec
- [x] Implementation: `agent_extra_env` for `codex` injects `OPENAI_API_KEY` from the host environment
- [x] Tests: `agent_extra_env` injects key; errors when key missing; `validate_agent_credentials` fails early if key not set
- [x] UX Review: `am start feat --agent codex` passes `OPENAI_API_KEY` into the container; clear error if key is not set

---

### Feature 8: Gemini Integration
> Spec: [`specs/gemini-integration.md`](specs/gemini-integration.md)

Auth mount preset for Google Gemini CLI.

**Fully implemented.** `KnownAgent::Gemini` is accepted, `~/.gemini` is mounted at `/home/<user>/.gemini` read-only, `validate_agent_credentials` checks the directory exists, and missing directories are silently skipped (no mount error).

- [x] Design: mount preset `~/.gemini:/home/<user>/.gemini:ro`; `validate_agent("gemini")` must pass
- [x] Tests: `resolve_agent_auth_mount("gemini")` returns correct host/container paths; mount included in `build_run_command`
- [x] Implementation: `gemini` branch in `resolve_agent_auth_mount()`
- [x] UX Review: `am start feat --agent gemini` launches a container with `~/.gemini` mounted read-only; graceful skip if `~/.gemini` doesn't exist

---

## Polish & Distribution
> Spec: [`specs/polish-and-distribution.md`](specs/polish-and-distribution.md)

### Integration test: full flow

**Done.** `tests/features/full_flow.feature` exercises `am init` → `am start` → `am list` → `am destroy` as a single end-to-end flow (plus a multi-session variant). The cucumber harness runs outside tmux with containers disabled by default.

### README

Write `README.md` at the repo root covering:
1. What it is — one-paragraph overview
2. Install — `cargo install --path .` and eventual binary download placeholder
3. Quick start — `am init` → `am start feat --agent claude` → `am attach feat` → `am destroy feat`
4. Configuration — pointer to `config.md`; minimal `~/.config/am/config.toml` example
5. Supported agents — table: claude, codex, copilot, gemini
6. Example Dockerfile — minimal image that installs `claude` and works with `am`

### Error message audit

**Done.** Every user-facing error in `src/error.rs` now states what went wrong and what to do next:

- `ContainerImageNotConfigured` → suggests setting an agent (`--agent`/`defaults.agent`) or `container.image`
- `ContainerRuntimeNotFound` → includes Podman and Docker install URLs
- `SlugAlreadyExists` → suggests `am attach <slug>` or `am destroy <slug>`
- `NotInTmux` → explains the command must run inside tmux (`tmux new-session` first)
- `SlugNotFound` → suggests `am list` to see valid slugs

### Cross-platform build verification

**Done.** `cargo build --release` is confirmed working on Linux x86_64 (local build:
`am --version` reports the crate version, all subcommands present). macOS arm64/x86_64 and
Windows are built by the `release.yml` matrix on every `v*` tag. Requirements and CI coverage
are documented in [`docs/reference/building.md`](docs/reference/building.md); the crate graph
is pure Rust with no system-library dependencies, so a stock toolchain suffices on every
platform.

---

## Bug Fixes

### Context-aware user messages
> Spec: [`specs/context-aware-messages.md`](specs/context-aware-messages.md)

Commands currently emit tmux-specific language (e.g. "kill tmux window") even when `$TMUX` is not set.

- Introduce a `Messages` trait (or `TmuxMessages`/`PlainMessages` structs) chosen once at startup from `tmux::is_in_tmux()`
- Thread it through command functions; remove any scattered inline `is_in_tmux()` checks used only for string selection
- Audit all `println!`, `eprintln!`, and confirmation prompts in command handlers
- Tests: `am destroy <slug> --force` outside tmux does not mention "window" or "pane"; inside tmux it does

---

## Architecture Audit Follow-ups

Actioned items from the 2026-07-12 architecture/usability audit. These are the
recommendations we *agreed* with; the "get back to basics" theme here means
**decoupling overloaded concepts**, not dropping the tmux/container/jj matrix
(that composition is the product's edge and stays first-class).

### Decouple command, integration, and image (highest priority)

Today a single `--agent` string means three things at once: the command that
launches (`main.rs` appends it as the container CMD), the auth preset
(`container.rs::resolve_agent_auth`), and the image (`config::resolve_image` via
`[agents.<name>]`). `KnownAgent::parse` rejects any name outside
`claude|copilot|gemini|codex` — even with `--no-container` — so there is no path
to "run this image, mount these creds, exec this command."

- [ ] Introduce independent concepts: **command** (what to exec), **integration**
      (which auth preset, if any), **image/profile** (runtime environment).
- [ ] Keep the current `--agent claude` shorthand as a preset that expands into
      the three, so nothing breaks for existing users.
- [ ] Make integration optional: an unknown/custom command with no preset should
      be allowed (this is what makes the tool genuinely "harness-agnostic").

### Custom-harness fast path

Deliver the mission promise ("quickly manage isolated AI harnesses") for
arbitrary tools, e.g. `am start idea --image my-image --cmd my-agent` with no
built-in integration required. Depends on the decoupling above.

### `am doctor` readiness check

Add a command that reports what's present/missing for a first successful
`am start`: repo + VCS, `.am/` initialized, container runtime, an available
image, tmux, and (per selected integration) required credentials/paths. Reuses
the existing preflight checks in `container.rs`. Preferred over silently
auto-bootstrapping `.am/` on `am start`.

### Session observability in `am list`

Session state is currently storage-oriented (enough for teardown/navigation).
Make `am list` operator-oriented by surfacing higher-level state: which
integration/command is active, whether the container is alive, whether the tmux
window still exists, and stale/broken markers.

### Docs: separate the fast path from the custom path; tone down future features

- [ ] Split docs into a **fast supported-integration path** and an
      **advanced/custom-harness path** so the two stories stop blurring.
- [ ] De-emphasize future `--team`/autonomous language and "everything ready to
      go" phrasing on primary surfaces until the core UX earns it. (README
      already lists prerequisites honestly; audit the docs site for the same.)

### Done

- **tmux window model.** `am start` inside tmux now creates a *dedicated* window
  (`new-window`) instead of renaming/splitting the caller's current window. This
  matches what the docs already described and makes start/attach symmetric.
  `destroy` kills the window outright; the `original_window_name` /
  `original_shell_dir` restore machinery is retained only to read session records
  written by older versions.

---

## Future (v2)

### Autonomous mode (`--auto` flag)

Add `--auto` to `am start`. In autonomous mode the agent works without waiting for user input between steps. Sets a flag in the session record; commit trailer becomes `Auto-Piloted-By`.

### Team orchestration (`--team` flag)

Add `--team` to `am start` to launch and coordinate multiple agents working toward a shared goal. Each agent gets its own isolated session and branch; `am` handles orchestration. Open questions: goal specification, result surfacing, how many agents and what slugs.

### Agent completion detection + OS notifications

Automatically detect when an agent pane exits or enters a waiting-for-input state and send an OS notification. Requires watching pane exit events via tmux hooks or a background thread.

### Per-session SSH deploy keys

Generate an SSH deploy key per session and inject it into the container, replacing the current `~/.ssh` read-only mount. Improves isolation and avoids exposing the user's main SSH credentials.

### Hooks

Run user-defined shell commands on session lifecycle events (start, attach, destroy, agent exit). Configured in project or global config.

### Versioned documentation

Add [`mike`](https://github.com/jimporter/mike) alongside the existing MkDocs setup to deploy versioned docs to GitHub Pages (e.g. `/0.1/`, `/0.2/`). Defer until breaking changes start appearing between minor versions or users begin pinning to older releases.

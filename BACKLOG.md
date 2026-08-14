# Backlog

Outstanding work for `am`. Items are grouped by theme and roughly ordered by priority.

---

## Agent Integrations

### Feature 7: Codex Integration
> Spec: [`specs/codex-integration.md`](specs/codex-integration.md)

Auth preset for OpenAI Codex — API key, interactive sign-in, or both.

**Fully implemented.** `KnownAgent::Codex` is accepted, `resolve_agent_auth_mounts` mounts `~/.codex` read-write when it exists, `agent_extra_env` injects `OPENAI_API_KEY` when it is set, and `validate_agent_credentials` fails early only when *neither* credential is present.

Originally shipped env-var-only, on the premise that codex authenticated solely through `OPENAI_API_KEY`. The CLI also supports interactive sign-in persisted to `~/.codex/auth.json`; users on that path had no credentials in the container and were forced to export a key they did not otherwise need.

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

## Onboarding

### Feature: Guided Setup (`am setup`)
> Spec: [`specs/guided-setup.md`](specs/guided-setup.md)

Interactive alternative to `am init` — a guided front door that asks only the questions
detected state can't answer, then verifies the result with `am doctor`'s own checks.

**Fully implemented.** `am setup [--yes] [--agent <name>]` runs `am init`'s setup, asks which
agent to use, whether to proceed with containers disabled (only when no container runtime is
found and there's a global config to write to), and a pane layout (always asked, unless `--yes`
or there's no global config to write to), writes the answers, and runs `doctor::run()` to
verify — offering to start a first session on success. Every question states where its answer
is saved — scope first, then the file path (e.g. `just this repo; saved to .am/config.toml.`)
— so a change's destination is never left to be inferred.

A follow-up readability pass restyled both `am init` and `am setup`'s output: `am init` moved
from a flat line list to a headline-plus-detail shape (`Initialized am in this repo.` /
`am is already initialized in this repo.`, with the detail indented underneath); `am setup`'s
own status and confirmation lines now shorten every path (project repo-relative, global
`~`-prefixed), and each question opens with its own header line, then the dimmed, indented
write-target and "currently: ..." lines below it, with blank lines separating phases —
`[1] claude  (already authenticated on this host)` shortened to `[1] claude    authenticated`.
Presentation only; no behavior, exit code, or write changed. `cargo clippy --all-targets --
-D warnings` is clean and `cargo test` passes (467 unit tests + 96 cucumber scenarios / 791
steps, 0 failed). Code review passed with all findings resolved.

- [x] `src/cli.rs`: `Commands::Setup { yes, agent }`
- [x] `main.rs`: `cmd_init`'s directory/`.gitignore` logic extracted into `init_project`, shared
      by `cmd_init` and `cmd_setup` so the two cannot drift apart
- [x] `Cargo.toml`: `toml_edit = "0.25"` — format-preserving edits to a config file the user may
      have hand-edited (comments, table order, and unrelated keys all survive)
- [x] `src/onboarding.rs`: `DetectedState::gather` (project/global/compiled-default precedence
      for `defaults.agent`, `container.enabled`, and the three `tmux.*` layout keys), the
      `Io`/`TermIo`/`ScriptedIo` seam, `ask_agent`/`ask_container_enabled`/`ask_layout`
      (`ask_layout_custom`'s direction-first sub-flow, `render_layout`'s ASCII previews), the
      shared `write_target_line`/`dim_line` helpers each question prints under its own header, the
      config skeletons, and the `toml_edit`-based `update_project_agent`/
      `update_global_container_enabled`/`update_global_tmux_layout` (no-op, byte-for-byte, when
      the requested value already matches, and per-key for the three layout keys, so a
      percentage-only change doesn't also touch already-correct `agent_pane`/`split` lines; a
      structural value — table, array, array-of-tables, inline table — is refused with an error
      rather than overwritten)
- [x] `main.rs::cmd_setup`: wires detection → prompts → the three writes → `doctor::run` →
      optional `cmd_start`; TTY-gated via `std::io::IsTerminal`; exit code matches `am doctor`'s
- [x] `tests/features/setup.feature`: fresh repo, inherited-from-global agent (UC2), `--yes`
      no-op on an already-configured repo, `--yes --agent` deterministic change and no-op,
      failing-doctor exit code, unknown-agent rejection before any write, non-TTY without
      `--yes`, `--yes` never touching pane layout (fresh or already-configured global config)
- [x] `tests/features/setup_interactive.feature`: the write-target line on all three questions,
      the customize sub-flow's direction-dependent wording (left/right vs. top/bottom), and the
      project-override caveat note
- [x] Docs: `docs/reference/commands.md` (`## am setup`), `docs/reference/configuration.md`
      ("Writing config with `am setup`"), `README.md` quick start

Deferred review items, not dropped:

- [ ] `resolved_agent_answer: Option<Option<container::KnownAgent>>` in `cmd_setup`
      (`src/main.rs`) is a nested `Option` whose two levels mean different things — "resolved
      without prompting yet?" and "is there a change to write?" A small named enum would make
      the states self-documenting.
- [ ] The structural-value error says "found a table or array" without distinguishing which of
      the four shapes. `toml_edit` exposes `Item::type_name()`/`Value::type_name()`, returning
      exactly `"table"`/`"array of tables"`/`"inline table"`/`"array"`, if the message is worth
      tightening later.
- [ ] `shorten_for_display` (`src/onboarding.rs`) produces an empty string when `path == base`
      (`WriteScope::Project`) or a trailing-slash `~/` (`WriteScope::Global`). Unreachable today
      since neither config path ever equals its base, but nothing pins that invariant.
- [ ] `src/onboarding.rs` is ~2900 lines. The natural seam if it grows: "Question 6: pane
      layout" (`render_layout` through `update_global_tmux_layout`, ~330 lines) plus "Skeleton
      cleanup" (~90 lines) extracted into `src/onboarding/layout.rs`.
- [ ] `strip_skeleton_example` (`src/onboarding.rs`) normalises a missing trailing newline when
      rewriting a file. Harmless for current callers (all files originate from `am`'s own
      skeleton), but not byte-preserving in that one respect.
- [ ] `wait_with_output_timeout` (`tests/cucumber.rs`) kills by pid via an external `kill -9`
      because `child` was moved into the worker thread. Theoretical pid-reuse TOCTOU at the
      timeout boundary; keeping `child` on the main thread and threading only the blocking read
      would allow `child.kill()` directly.

---

## Polish & Distribution
> Spec: [`specs/polish-and-distribution.md`](specs/polish-and-distribution.md)

### Integration test: full flow

**Done.** `tests/features/full_flow.feature` exercises `am init` → `am start` → `am list` → `am destroy` as a single end-to-end flow (plus a multi-session variant). The cucumber harness runs outside tmux with containers disabled by default.

### README

**Done.** `README.md` covers all six: overview, install, quick start, configuration,
supported agents, and an example Dockerfile. The configuration section points at
`docs/reference/configuration.md` rather than the `config.md` named here, which never
existed at the repo root.

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

### Documented MSRV is stale and unenforced

[`docs/reference/building.md:39`](docs/reference/building.md) states "Rust 1.70 or later", but
the crate graph has drifted well past that:

- **Building the binary requires 1.85** — `toml_parser` 1.1.3, `indexmap` 2.14.0,
  `clap_lex` 1.1.0, `cpufeatures` 0.3.0, and `hybrid-array` 0.4.14 all declare it.
- **Running the test suite requires 1.88** — `cucumber` 0.23, `gherkin` 0.16, and their
  `globset`/`ignore` dependencies.

So the real floor is 1.85 to build and 1.88 to contribute — both well above the documented
1.70.

The drift moves in *both* directions, which is what makes it worth enforcing rather than
just correcting once. Measured a day earlier, the binary floor was 1.88, pinned there by
`home` 0.5.12 arriving transitively through `which` 6.0.3. The routine `which` v8 bump
dropped `home` from the graph entirely (v8 depends only on `libc`), lowering the floor to
1.85 — a change no one asked for, reviewed, or recorded.

Nothing catches any of this because nothing enforces it: `Cargo.toml` has no `rust-version`
field, and every job in `ci.yml` uses `dtolnay/rust-toolchain@stable`, so CI has never once
compiled against the floor the docs promise. A user on 1.70 following those docs gets a
compile failure deep in a transitive dependency, with nothing pointing at the real cause.

- [ ] Set `rust-version` in `Cargo.toml` to the real floor so `cargo` reports a clear error
      instead of a dependency-resolution failure
- [ ] Correct `docs/reference/building.md` to match, and say whether the documented floor is
      the build floor or the contribute floor — they differ
- [ ] Add a CI job pinned to the declared MSRV, so the two cannot drift again
- [ ] Decide the policy: track stable and document it honestly, or hold a floor and pin the
      dependencies that push past it. Note the binding constraints have all been *transitive*
      so far — no direct dependency choice would have surfaced them

Found while pricing prompt-crate options for [`specs/guided-setup.md`](specs/guided-setup.md);
independent of that feature.

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
Note: this was originally recorded as blocking **Dev Container Support**, and it
turned out not to be. Devcontainer mode simply never resolves an `am` image —
`config::resolve_image` is now called from exactly one place, `plan_image` — so
`--agent claude` already stops implying an image on that path. The decoupling is
still worth doing for the custom-harness fast path below; it is not a prerequisite
for anything shipped.

### Custom-harness fast path

Deliver the mission promise ("quickly manage isolated AI harnesses") for
arbitrary tools, e.g. `am start idea --image my-image --cmd my-agent` with no
built-in integration required. Depends on the decoupling above.

### `am doctor` readiness check

**Done.** `src/doctor.rs` reports repo + VCS, `.am/` initialization, git identity, tmux,
container runtime, environment source, and per-agent credentials — and in devcontainer
mode the CLI and its version, Node ≥ 20, the discovered config, whether the built image
is current for the config hash, and constructs `am` refuses (`dockerComposeFile`) or drops
(`initializeCommand`, `runArgs`).

The checks call the same functions `cmd_start` does (`detect_runtime`,
`validate_agent_credentials`, `devcontainer::find_config`), so a passing report and a
working `am start` cannot drift apart. Exits 1 on any failure, so it gates a setup script;
warnings alone do not fail. It mutates nothing, which is what makes it the alternative to
auto-bootstrapping `.am/` as a side effect of `am start`.

Not covered: `privileged` and `capAdd` are label-only properties, so they cannot be
reported until the image has been built. Only the config-visible gated constructs
(`initializeCommand`, `runArgs`) are checked.

### Session observability in `am list`

Session state is currently storage-oriented (enough for teardown/navigation).
Make `am list` operator-oriented by surfacing higher-level state: which
integration/command is active, whether the container is alive, whether the tmux
window still exists, and stale/broken markers.

Partly landed: `am list --all` reports a `stale` marker when a session's repository no
longer exists, and `am session rm` cleans those records up. Still storage-derived,
though — the marker is a `Path::exists` check on the record, not a liveness probe. The
container and tmux window are still never queried.

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

## Dev Container Support

Full plan: [`specs/devcontainer-support.md`](specs/devcontainer-support.md).

Let a session's environment come from the repo's own `.devcontainer/devcontainer.json`
instead of an `am`-specific image, so projects stop maintaining a second, `am`-shaped
image alongside the one they already describe for editors and CI.

Design is a **build/run split**: delegate `devcontainer build` to the reference CLI (the
half with all the complexity and churn — OCI feature resolution, install ordering,
Dockerfile generation), then run the resulting image with `am`'s existing mount, user,
network, and SELinux machinery in `container.rs`. Because `am` keeps the run path,
host-path mirroring still applies and **both git worktrees and jj workspaces work** with
no CLI-side workarounds. Images are keyed by a config hash, so the Node CLI runs once per
config change rather than once per session.

- [x] **Phase 0 — spike.** Done 2026-08-09 against CLI 0.88.0 + podman; results and their
      implementation consequences are in the spec's *Spike results*. Both technical questions
      came back favorable: the run path is Node-free after build, and `build` exits 1 on
      error.
- [x] **Phase 1.** Done. `container.mode` selection, discovery in the worktree, config-hash
      image caching, build step, metadata/JSON merge, run step, lifecycle hooks, agent
      injection, trust gate, session state, docs — git and jj both. `container.mode`
      defaults to `"auto"`: a repo that describes its environment means for that
      description to be used, and repos without a `.devcontainer/` are unaffected because
      `auto` falls back to an image. `mode = "image"` is the opt-out.
- [ ] **Phase 2.** `userEnvProbe`, `forwardPorts`, and vendoring the CLI bundle if
      `npm install -g @devcontainers/cli` proves to be real friction (it is one
      dependency-free 1.7 MB script, so this is cheap).
- [ ] **Phase 3.** Docker Compose configs (`dockerComposeFile`), if worth owning.
- [ ] **Optional.** Replace the build step with a native Rust feature-builder, ideally as
      its own crate — crates.io has no devcontainer runtime today. The run path is
      unaffected by design.

Prerequisites this surfaced, both now done and both a fix in their own right:

- [x] `WorktreeGuard` with an explicit `commit()`. A failed `am start` now rolls the
      worktree *and its branch* back instead of leaving an orphan that `am destroy` could
      not see, so the same slug is immediately reusable.
- [x] Split `cmd_start` preflight into pre-worktree checks (runtime, credentials, slug) and
      post-worktree checks, since a devcontainer config can only be read after the worktree
      exists.

Follow-ups phase 1 left behind:

- [ ] `postAttachCommand` is never run — `am attach` moves tmux focus rather than attaching
      to the container, so there is no attach event to hang it off. Needs a real
      "exec into the running container" attach path.
- [ ] Create-time lifecycle hooks re-run on every `am start` because containers are `--rm`.
      Correct given ephemeral containers, but a persistent-container mode would make the
      spec's once-per-container semantics achievable; `lifecycle_done` already records what
      ran.
- [ ] The config hash does not cover build-context files, only the config and the
      Dockerfile. `--rebuild` is the workaround; a bounded context fingerprint would be
      better.

---

## Open follow-ups (2026-08-11)

Left behind by the config, container-lifecycle, and output work of 2026-08-11.

- [ ] **Decide whether a session worktree should use its own `.am/config.toml`.**
      `find_repo_root` deliberately walks past worktrees and jj workspaces to the main
      repository, so only the root copy is ever read — but `.am/config.toml` is committed,
      which puts an inert copy in every worktree. `am doctor` now warns that the file is
      never read; the underlying question of which file *should* win is unanswered.
- [ ] **Verify Codex end to end.** The `~/.codex` mount and the two-credential preflight
      are covered by unit tests, but no authenticated Codex session has been run — the
      dev container has no `codex` binary. Needs one real `am start <slug> --agent codex`.
- [ ] **Confirm the `container.agent` removal before release.** It is committed as
      `feat(config)!` with a `BREAKING CHANGE:` footer. The key was a second name for
      `defaults.agent` that inverted the documented precedence, and a stale one is now
      reported by the unknown-key warning rather than silently ignored — but it is still a
      config key that existed in a shipped version.
- [ ] **Move the exec-mock tests to their own test target.** The suite is serialized via
      `RUST_TEST_THREADS = "1"` in `.cargo/config.toml` because a test that writes a mock
      executable and immediately execs it races with any other thread that forks. Splitting
      those tests into a separate target would let the rest run in parallel again; the
      serialization is the cheap fix, not the structural one.

Decided against, recorded so it is not re-proposed:

- **`deny_unknown_fields` on the config structs.** Unknown keys are warned about instead.
  Rejecting them would break a teammate running an older `am` the moment someone commits a
  key their binary predates, and `.am/config.toml` is meant to be committed and shared. It
  would also turn every future key removal into a breaking change.
- **`am init` warning about unknown config keys.** The config it writes is fully commented
  out and cannot be wrong yet, so the warning would only ever fire when re-running `init`
  over an existing file. The real gap it exposed — `am attach` loading config without
  warning — was fixed instead by routing every load through one helper.

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

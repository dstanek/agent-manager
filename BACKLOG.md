# Backlog

Outstanding work for `am`. Items are grouped by theme and roughly ordered by priority.

---

## Session Recovery

### Restore the Agent on `am attach`
> Spec: [`specs/attach-restore-agent.md`](specs/attach-restore-agent.md)

**Done.** Before this, `am attach` only ever restored the tmux *window* — a reboot survives at
the worktree and session-record level, but tmux and the agent process do not, and `am attach`
recreated an empty window (or, for a container session, printed a `Note:` telling you to run
`am destroy --force <slug> && am start <slug>` by hand instead of fixing it).

`Session.agent` is now recorded by `am start` and kept current by `am run`, so `am attach` knows
what to relaunch. `am attach` checks the pane's actual state (`tmux::pane_current_command`,
biased hard toward "still running" on any ambiguity — a missed relaunch costs an `am run`, an
unwanted one can interrupt live work) and does the least work needed to fix it: an already-live
session is an unchanged, instant no-op; a live window with an idle agent pane gets a
`send-keys` relaunch in place; a fully gone window gets recreated (window, split, and — for a
containerized session — the container itself, via a container-planning helper shared with `am
start` so the two can't drift apart) before the agent is relaunched into it. Resume is on by
default (`--continue` for Claude/Copilot, `--resume latest` for Gemini, `resume --last` for
Codex, each verified against the CLI's own `--help`), with `am attach --fresh` and
`[attach].resume = false` as opt-outs. A legacy record with no agent falls back to `cfg.agent`
and persists the resolved value; if neither is available, `am` says so and points at `am run`.

Two accepted trade-offs: recreating a container re-runs `am start`'s own preflight (credential
validation, an image rebuild if pruned), so attach can now be as slow as, and fail the same ways
as, `am start` — only when the container is genuinely gone. And if that preflight fails, the
early return skips the shell-pane selection, so you land on the agent pane rather than the shell
pane; the window and split are created first, so a retry has something real to act on.

`cargo test` (528 unit tests + 120 cucumber scenarios) and
`cargo clippy --all-targets -- -D warnings` both clean.

Deferred code-review suggestions, not dropped:

- [ ] The host relaunch path writes the session record twice
      (`session::update_session_global`) where once would do — a redundant file write and lock
      acquisition in the common case, harmless but avoidable.
- [ ] A failed container-recreate preflight leaves focus on the agent pane instead of the shell
      pane (see above) — the early return skips `select_pane`. Cosmetic; a retry corrects it.
- [ ] `agent_command` appends auto flags before resume flags; harmless today because no agent
      combines a non-empty `agent_auto_flags` with a subcommand-shaped resume form (like
      Codex's `resume --last`, which must be the first token), but latent breakage for a future
      agent that has both. Already flagged with an in-code comment at the call site.

### Credential preflight checks presence, not validity

**Open.** `container::validate_agent_credentials` only checks that an agent's credential *path*
exists. An expired, revoked, or logged-out credential leaves that path in place, so preflight
passes and `am` reports success while the agent fails to authenticate inside the pane. Per agent,
the entire check is:

| Agent | What is checked |
|---|---|
| Claude | `$CLAUDE_CONFIG_DIR`, else `~/.claude`, exists |
| Copilot | `~/.config/gh` exists |
| Gemini | `~/.gemini` exists |
| Codex | `~/.codex/auth.json` exists, **or** `OPENAI_API_KEY` is set and non-empty |

This is pre-existing behaviour inherited from `am start` — `am attach`'s container recreate
reuses the same preflight via `plan_container_runtime`. What is new is that `am attach` now
reaches this path at all, and that its success line actively asserts a recovery that may not have
happened.

**How to reach the broken state.** With a containerized session (`container.enabled = true`):

1. `am start feat --agent claude`. The record gets `agent = "claude"` and a `SessionContainer`.
2. Let the credential expire or revoke it — sign out on another machine, or simply wait out the
   token lifetime. **Do not delete `~/.claude`**; sign-out and expiry both leave the directory
   behind, and that is the entire point of the bug.
3. Reboot, or otherwise kill tmux and the container. The worktree and `sessions.json` survive.
4. `am attach feat`.

Observed: `select_window` fails, so `recreate_attach_window` creates the window and split,
persists the pane targets, and calls `attach_recreate_container_cmd` →
`plan_container_runtime` → `validate_agent_credentials(Claude)` →
`ensure_required_paths(&[~/.claude])`. The path exists, so the check passes. The run command is
built and `send_keys`'d into the pane, and `am` prints

```
Opened new window for session 'feat' and restarted the container.
```

and exits `0`. The container starts, `claude --continue` runs inside it, and the authentication
failure surfaces only as agent output in the pane — never as an `am` error, and never in the exit
status. Anything scripting `am attach` sees a clean success.

Contrast with the cases preflight *does* catch, which fail loudly and are recoverable: a
container runtime that is not up yet (the common post-reboot case, caught by `detect_runtime`)
or a credential directory that is genuinely absent. Both error after the window and split exist,
and a retry after fixing the cause goes through `relaunch_into_existing_window` — the pane reads
as a bare shell, so `agent_pane_status` returns `Idle` — and completes normally.

Possible directions, none obviously right:

- [ ] Have `am doctor` check credential *validity* rather than presence, and point `am attach` at
      it on failure. Requires a per-agent liveness probe, which is agent-CLI-specific and may
      require a network round-trip.
- [ ] Do not claim recovery in the success line when `am` cannot confirm the agent came up —
      soften `... and restarted the container.` to describe what was actually done.
- [ ] Accept and document only. The failure is visible in the pane the user is looking at; the
      real exposure is scripted/unattended use.

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

A second pass carried the same treatment to `am start` and `am attach`. `am start`'s indented
detail lines (`worktree:`, `branch:`, `container:`, `image:`) are now dimmed, and `worktree:`
is shortened relative to the repo root instead of printing the full absolute path; the two
near-duplicate renderers at its two call sites (the exec-without-tmux early return and the
normal in-tmux return) were unified into one, `start_detail_lines`, with no change to which
fields either path prints. `am attach`'s `Note:` line, printed when a stopped session's window
is recreated, now goes through the shared `note_prefix()` instead of a hardcoded string, so it
carries the same yellow severity as every other note in `am`; the `To restart cleanly: ...` line
beneath it is dimmed. Presentation only; no behavior, exit code, or write changed in either pass.

A third pass, an on-ramp revision (spec: [Resolved Decisions](specs/guided-setup.md#resolved-decisions)
#10–#14), responded to feedback that `am setup` was "a strong guided configuration command, but
not yet a complete first-time-user on-ramp." Five changes: readiness (`doctor::run()`) now runs
before the pane-layout question instead of after it, so a first-time user is never asked to pick
pane proportions before knowing whether the tool can even start a session — agent → containers →
doctor's checks → *(only if clean)* layout → first session; a failing run now ends with a
"What to do next:" block that re-lists every failing check's own hint as a flat checklist,
replacing the old "Fix the items above, then re-run 'am setup'." dead end — the hints themselves
were strengthened in `doctor.rs`/`container.rs` (concrete install links for a missing runtime,
`container::credentials_hint` naming each agent's actual sign-in command, a concrete example for
a missing image), so `am doctor` gets the identical improvement for free; a brand-new machine
(no `~/.config/am/config.toml` yet) is now asked once, explicitly, whether it wants isolated
containers at all — recommended, defaulted to yes — rather than the choice being made implicitly
by whatever happened to be on `PATH`; the agent menu's per-agent note changed from
`"authenticated"` to `"credentials found"`, matching `doctor::check_agent`'s own presence-only
"present" wording, with an explicit `claude` fallback line when nothing is configured or
credentialed anywhere; and the docs' quick-start path now leads with `am setup` instead of
`am init`, with `am init` retained as the later, scriptable-path step.

`cargo clippy --all-targets -- -D warnings` is clean in all four colour environments
(default, `NO_COLOR=1`, `CLICOLOR_FORCE=1`, both together) and `cargo test` passes
(487 unit tests + 107 cucumber scenarios / 887 steps, 0 failed) as of the on-ramp pass.
Code review passed for all three passes, with all findings resolved — the second pass's only
finding was that `am attach`'s headline had nothing pinning it as plain, proven by wrapping it
in `dim_line` and watching the whole suite stay green; it is now covered.

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
- [x] `main.rs::cmd_setup`: moved the layout question and its write-back from before
      verification to after it, gated on `report.failures() == 0`; `print_what_to_do_next`
      replaces the old one-line failure message
- [x] `src/onboarding.rs`: `ask_container_consent` (informed-consent framing on a fresh setup),
      wired in `cmd_setup` to branch on `detected.global_config_exists` against the unchanged
      `ask_container_enabled`; `ask_agent`'s menu note changed to "credentials found" plus the
      explicit `claude`-fallback line
- [x] `src/doctor.rs` + `src/container.rs`: `container::credentials_hint` (per-agent, used by
      `check_agent`'s `Status::Fail` hint), concrete install links added to `check_runtime`'s
      hint, a concrete example added to `check_image_mode`'s hint
- [x] `tests/features/setup.feature` + `setup_interactive.feature`: ordering (layout never
      reached on a failing report; "Checking your setup..." precedes "Which layout do you
      want?" on a clean one), the "What to do next:" block, the consent question's two framings
      (fresh + runtime present, fresh + no runtime, returning-setup unaffected)
- [x] Docs: `docs/getting-started/quick-start.md` (`am setup` promoted to Step 1, `am init`
      retained as the scriptable-path tip), `docs/reference/commands.md` (`## am setup`'s
      question order, container consent question, and failure ending; `## am doctor`'s
      strengthened hints)

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

### Severity-prefix call sites with no message-content test

`color.rs`'s `error_prefix`/`warning_prefix`/`note_prefix` moved out of hand-rolled per-site
`eprintln!` calls and into one shared, unit-tested home. Most call sites picked up test
coverage for free because they already had a test asserting the printed message — but seven
did not, and still don't:

- [`container.rs:934`](src/container.rs) — `warning_prefix`
- [`devcontainer.rs:1005,1011,1017`](src/devcontainer.rs) — `note_prefix`, three sites sharing
  one `let note = …` binding
- [`worktree.rs:242`](src/worktree.rs) — `warning_prefix`
- [`session.rs:262,273`](src/session.rs) — `warning_prefix`, two sites

Each is a bare `eprintln!` with no test seam: nothing captures stderr for these code paths, so
there's no assertion to have carried the prefix call over during the move. `color.rs`'s own
unit tests cover the three prefix functions in isolation (uncolored and colored forms), and
that coverage is real — but it proves the functions behave correctly, not that any particular
call site still calls them. A change that dropped or misspelled the prefix at any of these
seven would compile, pass every existing test, and only be caught by someone reading the
diff.

- [ ] Give these sites a stderr-capture seam — most other `eprintln!`-based messages in the
      codebase go through a mockable stream or a test double already; extend whichever pattern
      is closest rather than inventing a new one
- [ ] Once captured, assert the exact prefixed line (not a substring of the message) at each
      of the seven sites, the way [`onboarding.rs`](src/onboarding.rs)'s `ask_layout` caveat
      test does
- [ ] Cover both the `color = true` and `color = false` paths — several of these sites, like
      that same `ask_layout` caveat before it was fixed, may only ever be exercised uncolored
      today

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

Design is a **build/run split**: something builds the image, then `am` runs it with its
existing mount, user, network, and SELinux machinery in `container.rs`. Because `am` keeps
the run path, host-path mirroring still applies and **both git worktrees and jj workspaces
work** with no builder-side workarounds. Images are keyed by a config hash, so a build
happens once per config change rather than once per session.

The build half began as pure delegation to the reference CLI and is now `am`'s own for most
configs. Both builders emit the same `devcontainer.metadata` label, which is the entire
contract between the two halves — so the run path never learned which one produced the
image, and the split is what made replacing the build half a contained change.

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
- [x] **Native builder.** Done 2026-08-15. `am` builds the image itself for a base `image`
      or a `build.dockerfile`, plus Features pulled from an OCI registry and ordered by
      `installsAfter` with an alphabetical tie-break — no Node, no `@devcontainers/cli`.
      `devcontainer.builder` chooses: `"auto"` (default) falls back to the CLI and names the
      construct that forced it, `"cli"` always delegates, and `"native"` turns a fallback
      into an error, for users who want a guarantee that no config silently reintroduces
      Node. Correctness is pinned by `#[ignore]`d differential tests that build the same
      config both ways and compare the resulting label. Not its own crate yet — crates.io
      still has no devcontainer runtime — but the seam is there if extracting it is worth it.
- [x] **Phase 2.** Done 2026-08-15. `forwardPorts` publishes each port on `127.0.0.1` — a
      deliberate divergence, since the reference CLI publishes nothing and leaves forwarding to
      an editor, and `am` has none. `userEnvProbe` runs the container user's login shell,
      captures its environment and applies it, skipping the variables `am` set on purpose;
      defaulting to `loginInteractiveShell` per the spec, so it applies unless a config opts
      out. Vendoring the CLI bundle was the third item here; the native builder removed the
      friction that was meant to relieve.
- [x] **Phase 3 — compose.** Done 2026-08-15. `dockerComposeFile` configs now bring their
      project up, run the agent in the named `service`, and go down on `am destroy`. The build
      half was nearly free — the service's image with Features baked in, same label; the run
      half is a second run model beside `build_run_command`. `am` parses no YAML: the resolved
      model comes from `compose config --format json`, and the override it contributes is
      written as JSON, which compose accepts because JSON is valid YAML.

Prerequisites this surfaced, both now done and both a fix in their own right:

- [x] `WorktreeGuard` with an explicit `commit()`. A failed `am start` now rolls the
      worktree *and its branch* back instead of leaving an orphan that `am destroy` could
      not see, so the same slug is immediately reusable.
- [x] Split `cmd_start` preflight into pre-worktree checks (runtime, credentials, slug) and
      post-worktree checks, since a devcontainer config can only be read after the worktree
      exists.

Follow-ups phase 1 left behind:

- [x] `postAttachCommand`. Done 2026-08-16. Reached two ways, because it is the one hook that
      is not tied to creating a container: chained into the command when one is being created
      (`am start`, and the attach paths that recreate a gone container), and `exec`'d into the
      running container when `am attach` finds a live session. The hooks for the second route
      come from the image's metadata label rather than a re-read of the config, since the label
      is what describes the container that is actually running. Best-effort: it never turns an
      attach to a working window into a failure, and a config without the hook execs nothing.
- [ ] Create-time lifecycle hooks re-run on every `am start` because containers are `--rm`.
      Correct given ephemeral containers, but a persistent-container mode would make the
      spec's once-per-container semantics achievable; `lifecycle_done` already records what
      ran.
- [ ] The config hash does not cover build-context files, only the config and the
      Dockerfile. `--rebuild` is the workaround; a bounded context fingerprint would be
      better. The native builder now knows the context and could close this with a git-aware
      hash of tracked files under it, but the fix belongs to both builders, so it stayed out
      of that change.

**The Node dependency is gone.** As of 2026-08-16 there is no config shape that sends `am` to
`@devcontainers/cli`. The last one was not a construct at all — it was a config naming nothing
to build from, which the reference CLI rejects too, so it became an error instead of a
delegation. `builder = "cli"` survives as an escape hatch nothing selects automatically.

Follow-ups the native builder left behind:

- [x] **`dependsOn`.** Done 2026-08-15. Hard dependencies resolve recursively — a worklist
      over manifests, so walking the graph costs one small GET per node and downloads no
      layers. Identity is contents-plus-options per the spec, so a diamond installs once and
      a cycle terminates. Doing this properly meant implementing the spec's round-based
      ordering, which **fixed an ordering bug in the previous release**: order was computed
      one eligible Feature at a time, which diverges from the CLI for any config with two
      independent `installsAfter` chains — that is, most real configs. `installsAfter` is in
      nearly every published Feature and `dependsOn` in almost none (15 popular Features
      checked, none used it), so the incidental fix is worth more than the feature.
- [ ] **`dependsOn` is not differentially tested.** The recursive walk now has offline unit
      tests via local Features — transitive pull-in, diamond dedup, cycle termination — but
      no Feature in the common registries declares `dependsOn`, so the two implementations
      have never been compared on one. Needs a Feature published for the purpose.
- [ ] **Registry auth is anonymous only.** Private Feature registries need `docker
      config.json` credentials and credential helpers. This one does not degrade gracefully:
      a private ref is still a registry ref, so it is never handed to the CLI and instead
      fails with the registry's own 401 text.
- [x] **`overrideFeatureInstallOrder`.** Done 2026-08-15, as the spec's `roundPriority` on
      top of the round machinery `dependsOn` added. It is a priority rather than an order:
      it cannot make a Feature jump a dependency, and raising one Feature *splits* its round
      and defers its round-mates. Both behaviours are pinned against the reference CLI.
- [x] **Features referenced by local path or tarball URL.** Done 2026-08-15. All three
      sources the spec defines now work; `dockerComposeFile` is the only fallback left. They
      differ only in where the bytes come from — once a Feature's directory exists, options,
      ordering, staging and the label are the same code. Identity follows the spec per
      source: layer digest for a registry Feature, the resolved path for a local one (the
      spec says every local Feature is distinct), and a hash of the bytes for a tarball.
- [ ] **Compose sessions are only tested against a mock runtime.** The build half has a
      differential test against the reference CLI, and the generated override is validated
      against real `docker compose config`, but no test brings a real project up and execs an
      agent into it — that needs a runtime, a tmux session and a live agent at once.
- [ ] **`podman compose` is untested.** The code shells out to `<runtime> compose`, which
      podman 4+ provides, but the dev container has no podman to check it against.
- [ ] **A compose session whose service exits** leaves the project up until `am destroy`.
- [ ] **`postAttachCommand` runs once per `am attach` invocation**, not once per human attach —
      tmux has no "user looked at this window" event. Idempotent hooks are unaffected.
- [ ] **The env probe truncates a variable whose value contains a newline.** It converts the
      NUL-separated `/proc/self/environ` to lines; the reference CLI parses the NUL stream.
- [ ] **`portsAttributes`/`otherPortsAttributes` are carried in the label but not acted on.**
      They describe a port to an editor; `am` has no equivalent behaviour to apply.
- [ ] **A forwarded port that is already bound fails at session start**, not in preflight.
- [ ] **Tarball Features are not differentially tested.** The CLI's resolver accepts one from
      a local TLS server but its build path will not fetch from one, so no reference label
      can be produced locally. Unpacking has unit tests and everything after the fetch is
      shared with the other two sources; the HTTP fetch itself is untested.
- [ ] **A typo'd `overrideFeatureInstallOrder` entry is ignored, not an error.** The CLI
      resolves every entry and fails if it cannot; `am` only matches against the Features
      being installed. No effect on the label, so it is a diagnostics gap rather than a
      correctness one — closing it costs a network round trip per entry.

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

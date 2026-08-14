# Feature: Restore the Agent on `am attach`

## Feature Overview

**Problem.** `am attach <slug>` only restores the tmux *window*. It never restores the
*agent*. Today, when a session's tmux window is gone — most commonly because the machine
rebooted, but also if a user closed the window by hand — `cmd_attach` (`src/main.rs:1362`)
recreates the window and split, leaves both panes sitting at a bare shell, and stops. For a
containerized session it does not even recreate the container: it prints a `Note:` telling the
user to run `am destroy --force <slug> && am start <slug>` instead (`src/main.rs:1403-1417`).
The worktree survives a reboot; the agent's working context does not come back with it.

Root cause, confirmed in the codebase (see `src/main.rs`, `src/session.rs`):

1. `session::Session` never records which agent a session launched. `cmd_start` computes
   `effective_agent` (`src/main.rs:668`) and uses it to `send_keys` the agent into the pane or
   exec the container command, but never writes it into the `Session` it persists
   (`src/main.rs:790-811`). There is nothing on disk for `am attach` to restart even in
   principle.
2. `cmd_attach`'s window-recreate branch passes `None` as the split's command argument
   (`src/main.rs:1394`), unlike `cmd_start`'s equivalent split (`src/main.rs:761-768`), which
   passes `container_shell_cmd`. The container branch explicitly punts with a `Note:` instead
   of rebuilding anything.

**User value.** A user whose machine reboots (or who closes an `am` tmux window by accident)
gets back to exactly where they left off with a single `am attach <slug>` — window, pane
layout, container (if any), and the agent process itself, ideally picking up its previous
conversation rather than starting cold.

**Success criteria.**
- `am attach <slug>` on a session whose tmux window and agent process are both gone (the
  reboot case) recreates the window/split *and* relaunches the recorded agent, by default
  asking it to resume its previous conversation.
- This works for both host-path and containerized sessions.
- Attaching to a session that is still fully alive (window present, agent still running in
  its pane) remains an instant no-op — no relaunch, no extra container/credential work.
- Session records written before this feature (no `agent` field) degrade gracefully per
  [OQ-1](#oq-1-fallback-agent-for-pre-existing-session-records) rather than erroring or being
  silently ignored.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` pass, with cucumber coverage
  for every flow in the Use-Cases section.

## Assumptions

- **A1.** "Resume the previous conversation" means resuming the underlying agent CLI's own
  session/conversation state (e.g. its chat history), not anything `am` itself persists —
  `am` only ever forwards a CLI flag; it has no visibility into agent conversation state.
- **A2.** The container-recreate path re-runs the same preflight `am start` runs today
  (runtime detection, credential validation, devcontainer image build-or-reuse) — an attach
  that has to rebuild a container is allowed to be as slow as `am start`, since that only
  happens when the container is actually gone. The already-attached fast path stays cheap.
- **A3.** `am attach`'s window-recreate step is not wrapped in a rollback guard the way
  `WorktreeGuard` protects `am start`'s worktree creation (`src/main.rs:718`). If the agent
  relaunch or container recreate fails *after* the window/split already exist, the user is
  left with an opened window and a clear error — the same "partially succeeded, retry or use
  `am run`" state `am attach` already tolerates today. This is judged acceptable because,
  unlike a worktree, a tmux window is cheap to redo and carries no data.
- **A4.** Two concurrent `am attach <slug>` calls for the same session (e.g. two terminals
  racing right after a reboot) can both decide the window is missing and both try to
  recreate it/the container. This race already exists in `cmd_attach` today and is not
  introduced by this feature; `lock_global_sessions()` only serializes the session-record
  write, not the tmux/container side effects. Out of scope to fix here — noted under Edge
  Cases, not blocking.
- **A5.** `am run <slug> <agent>` remains the dedicated "launch a specific agent into a
  session" primitive. This feature does not add an `--agent` override flag to `am attach` —
  when no agent can be determined (see OQ-1), the user is pointed at `am run` instead of
  `am attach` growing a second way to say the same thing.

## Open Questions

Each has a recommended default; ship the default unless the user overrides it.

### OQ-1: Fallback agent for pre-existing session records

A session written before this ships has no `agent` field (defaults to `None` via serde). What
does `am attach` do for it?

**Recommendation:** fall back to the current `cfg.agent` from config, mirroring `cmd_start`'s
own precedence (`--agent` flag > `cfg.agent`) minus the flag, since attach has no `--agent`
flag (A5). If `cfg.agent` is also `None`, behave exactly as today: open the window/pane with
nothing launched, and add a one-line `Note:` pointing at `am run <slug> <agent>`. The first
time this fallback fires for a record, write the resolved agent back onto the session (`s.agent
= Some(...)`) so subsequent attaches use the recorded value instead of re-deriving it from
config, which may have changed since.

### OQ-2: Containerized path — recreate the container, or keep punting?

**Recommendation:** recreate it. Reuse the same planning logic `cmd_start` already has
(`plan_container`/`plan_image`/`plan_devcontainer`, `src/main.rs:946` onward) via a shared
helper callable from both `cmd_start` and `cmd_attach`, feeding it data recovered from the
`Session` record (`SessionContainer.runtime/image/mode/config_path/config_hash/remote_user`,
`Session.auto`, and the newly-recorded `Session.agent`) plus a freshly loaded `Config`. Call
`container::remove_if_exists` for the recorded container name first, exactly as `cmd_start`
does for a leftover container, then pass the rebuilt run command into `split_window`'s
`container_shell_cmd` argument instead of `None`.

*Trade-off against keeping today's `Note:`:* recreating is real preflight work — credential
validation can fail (expired token, revoked login), and a devcontainer whose image was pruned
rebuilds from scratch. That is strictly more that can go wrong on attach than today's "print a
hint and stop." The alternative (keep punting) is simpler and never surprises the user with a
slow attach, but leaves the exact problem the user reported unsolved for containerized
sessions — and `auto` mode requires a container, so a meaningful fraction of real sessions are
containerized. Recommend recreating; if the user disagrees, the fallback is to ship OQ-2 as
"host path only" for this PRD and keep today's `Note:` unchanged for containers.

### OQ-3: Per-agent resume invocation

**Not verified — do not assume.** `am` currently has no resume/continue logic anywhere, and
the four `KnownAgent` CLIs (`src/container.rs:39`) are not guaranteed to (a) support resuming
a previous conversation at all, or (b) use a comparable flag if they do. The team-lead's
instruction going in was explicit: treat this as a research spike, not a known fact.

**Recommendation:** the backend-engineer verifies each CLI's actual resume support and flag
(via `--help`/docs) before wiring `--fresh`/`[attach].resume` to anything, and builds a small
per-`KnownAgent` table, e.g. `agent_resume_flags(agent: KnownAgent) -> Option<Vec<String>>`
returning `None` for an agent confirmed to have no resume capability. Where `None`, `am`
silently falls back to a fresh launch — never errors just because resume isn't supported.
Land the plumbing (the `resume: bool` parameter threaded through `agent_command`, the config
key, the `--fresh` flag) in the same change, since none of it depends on the verified flags
being correct on day one, but do not merge unverified flag guesses.

### OQ-4: Default-on, opt-in flag, or config-driven?

**Recommendation:** default-on, with a per-invocation escape hatch and a config override —
not mutually exclusive:
- New `[attach]` config section, `resume: bool`, default `true`.
- New `am attach --fresh` flag forces a new conversation for this one invocation regardless
  of config.
- No flag to force resume when config has `resume = false` is needed in this PRD — a user who
  wants resume-by-default just leaves the default alone.

This directly satisfies the "bonus" ask (resume by default, no extra typing) while giving
users who don't want it — or who hit a flaky per-agent resume implementation — a way out
without editing config.

### OQ-5: No prior conversation to resume

**Recommendation:** `am` does not special-case this. The resume flag (once verified per OQ-3)
is passed to the agent CLI exactly as it would be by a human typing it directly; whatever that
CLI does when there is nothing to resume (start fresh, print a message, etc.) is what happens.
`am` cannot observe or react to the agent's exit status here — the agent is launched via
`tmux send_keys` into a live pane, fire-and-forget, exactly like `cmd_run` and `cmd_start`
already do today. This is a pre-existing constraint of the send-keys launch mechanism, not a
new gap this feature introduces.

### OQ-6: Detecting an already-running agent (avoid double-launch)

**Recommendation:** yes, required, and it doubles as the mechanism for
[UC-4](#uc-4-attach-to-a-live-session-whose-agent-exited). Add
`tmux::pane_current_command(pane_target: &str) -> Result<String>` (new function, wraps `tmux
display-message -p -t <target> '#{pane_current_command}'`). On the "window already exists"
fast path, query the agent pane's current foreground process before doing anything else:
- Container sessions: compare against the container runtime binary name already on the
  record (`SessionContainer.runtime`, e.g. `"podman"`/`"docker"`) — while the container is up,
  that's the pane's foreground process regardless of what's running inside it; if the agent
  process inside the container exits, the container exits too (it's the container's PID 1),
  and the pane reverts to a shell. No new per-agent process-name table needed for this case.
- Host sessions: compare against the recorded agent name. Some agent CLIs run under an
  interpreter shim (`node`, `python`, …) rather than a process literally named e.g. `claude`,
  so an exact-name match can false-negative. **Bias safe:** on any ambiguity (unrecognized
  process name, lookup failure, pane target gone), treat it as "still running, do nothing" —
  a missed relaunch is a minor inconvenience the user can fix with `am run`; an unwanted
  relaunch on top of a live agent is much worse (interrupts a working session, possibly loses
  in-flight state). Only relaunch when the pane is confidently idle at a shell prompt.

## Use-Cases

### UC-1: Post-reboot recovery — host (non-container) session

**Actor:** developer whose machine rebooted; tmux and the agent process are gone, the git
worktree and session record survive.

**Preconditions:** a `Session` record exists for `<slug>` with `container: None`;
`session.agent` is `Some("claude")` (or resolved via OQ-1); the developer is inside a live
tmux server (freshly started since the reboot).

**Main flow:**
1. `am attach <slug>`.
2. `tmux::select_window` fails (window doesn't exist).
3. `am` creates the window and split exactly as today (`create_window` + `split_window`).
4. `am` resolves the launch command for `session.agent`, including the resume flag unless
   `--fresh` was passed or `[attach].resume = false`.
5. `am` sends that command into the new agent pane via `tmux::send_keys`.
6. `am` selects the shell pane (unchanged from today) and updates the session record's tmux
   fields (unchanged from today).
7. Prints e.g. `Opened new window for session '<slug>' and relaunched 'claude' (resuming).`

**Postconditions:** window, split, and agent process all restored; agent pane shows the agent
attempting to resume its last conversation in the worktree directory.

**Business rules:** the launch command is built the same way `cmd_start`'s host path builds
it (`send_keys(&pane, agent)`), with the resume flag appended per OQ-3/OQ-4.

### UC-2: Post-reboot recovery — containerized session

**Actor:** same as UC-1, but the session was started with a container (image or devcontainer
mode; `auto` or not).

**Preconditions:** `Session.container` is `Some(SessionContainer { .. })` with enough
recorded (`runtime`, `image`, `mode`, `config_path`, `config_hash`, `remote_user`,
`container_name`) to rebuild the run command; `Session.agent` is known (recorded or OQ-1
fallback).

**Main flow:**
1. `am attach <slug>`.
2. `tmux::select_window` fails.
3. `am` creates the window and split (unchanged).
4. `am` calls `container::remove_if_exists` for the recorded container name (defensive
   cleanup, mirroring `cmd_start`).
5. `am` rebuilds the container run command via the shared container-plan helper (OQ-2),
   including the resume-aware agent invocation as the container's command.
   - Devcontainer mode: if the image for the current config hash already exists locally, it
     is reused (no rebuild); if not, it is built, exactly as `am start` would.
6. `am` passes the rebuilt command into `split_window`'s `container_shell_cmd` argument
   (instead of `None`), so the split execs straight into the running container like
   `cmd_start`'s tmux path does.
7. Prints e.g. `Opened new window for session '<slug>' and restarted the container.`

**Alternative flow — image/build unavailable:** if `plan_container` fails (runtime not
running, credentials expired, devcontainer config deleted from the worktree since it was
built, etc.), `am` reports that error clearly. The window/split from step 3 already exist
(A3) — the user gets a clear failure message and can retry `am attach`, fix the underlying
problem (e.g. `podman machine start`, re-auth), or fall back to
`am destroy --force <slug> && am start <slug>` as before.

**Postconditions:** container, window, split, and agent all restored, or a clear actionable
error if any preflight step fails.

**Business rules:** never skip credential/runtime preflight just because this is "only" an
attach — the container is genuinely gone and must be validated exactly as `am start` would.

### UC-3: Attach to a still-live session (fast no-op path)

**Actor:** developer who already has the session's tmux window open (in this or another
client) and wants to jump back to it.

**Preconditions:** `tmux::select_window` succeeds; the agent pane's foreground process is
confidently the running agent (host) or the recorded container runtime (container).

**Main flow:**
1. `am attach <slug>`.
2. `tmux::select_window` succeeds.
3. `am` queries `pane_current_command(agent_pane)` and confirms the agent/container is
   running (OQ-6).
4. `am` prints `Attached to session '<slug>'.` — unchanged from today.

**Postconditions:** no relaunch, no container work, no extra latency beyond the one tmux
query added in step 3. This must stay the cheap, common-case path.

### UC-4: Attach to a live session whose agent exited

**Actor:** developer whose tmux window is still open, but the agent process inside the agent
pane crashed, was killed, or exited normally (e.g. `/exit`), leaving a bare shell.

**Preconditions:** `tmux::select_window` succeeds; `pane_current_command(agent_pane)` returns
a plain shell rather than the agent/container process.

**Main flow:**
1. `am attach <slug>`.
2. `tmux::select_window` succeeds.
3. `pane_current_command` shows the pane is idle.
4. `am` relaunches the recorded agent into the agent pane (resume-aware, same as UC-1 step
   4-5) — for a container session, only if the container itself is still up (otherwise this
   degenerates into UC-2, since a container's foreground exit ends the container).
5. Prints a message distinct from the plain no-op, e.g. `Attached to session '<slug>'; agent
   had exited — relaunched 'claude' (resuming).`

**Postconditions:** agent process restored in an otherwise-untouched window.

**Business rule:** relaunch only fires on a confident "not running" read (OQ-6); any
ambiguity defaults to UC-3's no-op instead.

### UC-5: Attach to a legacy session record (no agent ever recorded)

**Actor:** developer with a session created by an `am` build that predates this feature.

**Preconditions:** `Session.agent` is `None` (missing key in the on-disk JSON, defaulted by
serde).

**Main flow (window missing, `cfg.agent` set):** resolves per OQ-1 — falls back to
`cfg.agent`, proceeds as UC-1/UC-2, and persists the resolved agent onto the record.

**Alternative flow (window missing, `cfg.agent` also unset):** window/split created as today;
no agent to launch; prints today's message plus a new `Note:` — `am attach` does not know
which agent to launch — run 'am run <slug> <agent>'`.

**Postconditions:** never errors solely because the field is missing; degrades to the best
information available.

### UC-6: Force a fresh conversation

**Actor:** developer who wants a clean agent conversation instead of resuming (e.g. the last
conversation is irrelevant to what they're about to do).

**Main flow:** `am attach <slug> --fresh` — identical to UC-1/UC-2/UC-4's relaunch flows,
except the resume flag is omitted regardless of `[attach].resume`.

## Data Model

### `Session` (`src/session.rs:104`)

Add one field:

```rust
pub struct Session {
    // ...existing fields...
    /// The agent last launched into this session's agent pane (e.g. "claude"). Used by
    /// `am attach` to relaunch the agent after a reboot or a killed window. `None` for
    /// records written before this field existed, or for sessions started with no agent.
    #[serde(default)]
    pub agent: Option<String>,
}
```

- **Constraint:** free-form string, not `KnownAgent` — mirrors `cfg.agent: Option<String>`
  and the `--agent` flag, which both accept arbitrary strings validated lazily via
  `KnownAgent::parse` only where a `KnownAgent` is actually needed (auth preflight, resume
  flag lookup). An unrecognized string degrades to "launch it as a bare command, no
  resume/auto flags" rather than erroring, matching existing `agent_command` behavior when
  `KnownAgent::parse` fails at `cmd_start` time — note: `cmd_start` currently *does*
  `.transpose()?` on `KnownAgent::parse`, i.e. an unknown `--agent` value already hard-errors
  before any session is created, so a `cmd_start`-originated `Session.agent` is always a valid
  `KnownAgent` string. `cmd_run`'s `agent: String` CLI argument has **no such validator**
  (only `slug` has a `value_parser` in `src/cli.rs`), and `cmd_run` persists it onto
  `Session.agent` verbatim on success — so `am run <slug> some-typo` is an ordinary way to put
  an unparseable value on disk. `am attach` still `KnownAgent::parse`s it, but a parse failure
  there is an expected, reachable case reached via `cmd_run`, not a "should not happen"
  invariant — treat it as "degrade to no-resume-flag launch," full stop, not as a defensive
  fallback for something that can't occur.
- **Required for:** `cmd_start` (write), `cmd_run` (write — see Backend task list), `cmd_attach`
  (read + conditional write on OQ-1 fallback).
- **Backward compatibility:** `#[serde(default)]` — existing `sessions.json` records with no
  `agent` key deserialize as `None`; add a unit test alongside the existing
  `legacy_records_without_repo_root_migrate_correctly` test confirming this.

### `Config` (`src/config.rs:208`)

Add a new section:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachConfig {
    /// Whether `am attach` asks the agent to resume its previous conversation
    /// when relaunching it, instead of starting fresh. Overridden per-invocation
    /// by `am attach --fresh`.
    #[serde(default = "default_true")]
    pub resume: bool,
}

impl Default for AttachConfig {
    fn default() -> Self {
        Self { resume: true }
    }
}
```

with `pub attach: AttachConfig` added to `Config`, `#[serde(default)]`, and included in
`Config::default()` and the `am generate-config` template output (`cmd_generate_config`).

No new fields on `SessionContainer` — everything needed to rebuild a container run command
(OQ-2) is already recorded there.

## API Design

`am` is a CLI, not a network service; "API" here is the command/flag/config surface.

### `am attach <slug> [--fresh]`

| | |
|---|---|
| Auth | none (local CLI) |
| Preconditions | inside a repo `am` recognizes; inside a live tmux session (`AmError::NotInTmux` if not, unchanged); `<slug>` exists in the current repo's sessions (`AmError::SlugNotFound` if not, unchanged) |
| New flag | `--fresh` (bool, default `false`) — skip resuming; launch/relaunch with a fresh conversation regardless of `[attach].resume` |
| Success output | one of four messages depending on flow (UC-3 no-op / UC-1,4 relaunch / UC-2 container recreate / UC-5 no-agent-known), see each use-case |
| Error cases | `NotInTmux`, `SlugNotFound` (unchanged); new: container/credential preflight failure surfaces the underlying error with context (UC-2 alternative flow) — window/split already created (A3) |

### `am run <slug> <agent>` (existing, extended)

No new flags. Behavior change: on success, also updates `session.agent = Some(agent)` and
persists via `session::update_session_global`, so a subsequent `am attach` relaunches whatever
was actually run last, not stale data.

### Config: `[attach]` section

```toml
[attach]
resume = true   # am attach asks the agent to resume its previous conversation by default
```

Documented in `docs/reference/configuration.md`, included in `am generate-config` output with
its default and a one-line comment, same pattern as every other section there.

## Implementation Tasks

### backend-engineer

- [ ] Add `Session.agent: Option<String>` (`src/session.rs`), `#[serde(default)]`; update
      `make_session`/`make_session_for_repo` test helpers; add a roundtrip test with `agent`
      set and a legacy-record test confirming a missing `agent` key loads as `None`.
- [ ] Populate `new_session.agent = effective_agent.clone()` in `cmd_start`
      (`src/main.rs:790`).
- [ ] Update `cmd_run` (`src/main.rs:1426`) to set `s.agent = Some(agent.to_string())` and
      call `session::update_session_global` after a successful `send_keys`.
- [ ] Add `tmux::pane_current_command(pane_target: &str) -> Result<String>`
      (`src/tmux.rs`), wrapping `tmux display-message -p -t <target> '#{pane_current_command}'`;
      extend the `AM_TMUX_BIN` mock protocol and add unit/mocked coverage.
- [ ] Extract `cmd_start`'s container-planning block (runtime detection, credential
      preflight, `plan_container` call, leftover-container cleanup) into a helper reusable
      from both `cmd_start` and `cmd_attach`, parameterized so `cmd_attach` can feed it data
      recovered from a `Session` record instead of fresh CLI flags.
- [ ] **Spike (blocking, see OQ-3):** verify, per `KnownAgent` variant, whether the CLI
      supports resuming a previous conversation and what flag(s) that requires. Encode the
      result as `agent_resume_flags(agent: KnownAgent) -> Option<Vec<String>>`, returning
      `None` for any agent confirmed not to support it.
- [ ] Extend `agent_command` (`src/main.rs:1158`) — or add a sibling — to take a `resume:
      bool` and append `agent_resume_flags(agent)` when `Some` and `resume` is true.
- [ ] Add `AttachConfig`/`Config.attach` (`src/config.rs`) per Data Model; wire into
      `Config::default()` and `cmd_generate_config`'s template output.
- [ ] Add `--fresh` flag to `Commands::Attach` (`src/cli.rs`).
- [ ] Rewrite `cmd_attach` (`src/main.rs:1362`) per the flows in UC-1 through UC-6: fast-path
      `pane_current_command` check (OQ-6) with the "ambiguous → no-op" safety bias; OQ-1
      fallback-and-persist for `Session.agent == None`; host relaunch; container
      recreate-via-shared-helper (retiring today's `Note:`/manual-restart text for the case
      it now handles automatically — keep an error-path message pointing at
      `am destroy --force && am start` as the manual fallback when preflight fails, per A3).
- [ ] `cargo test` and `cargo clippy --all-targets -- -D warnings` clean.

### integration-tester

- [ ] New `tests/attach_restore_agent.feature` covering, with `AM_TMUX_BIN` /
      `AM_PODMAN_BIN`/`AM_DOCKER_BIN` mocks:
  - UC-1: window missing, host session, agent relaunched with resume flag by default.
  - UC-6: same, with `--fresh` — assert the resume flag is absent (per CLAUDE.md's Gherkin
    guidance: verify the *literal* absence via a dedicated Rust step or a single-quoted
    string if the expected text needs a `"`; do not rely on a `{string}` capture around any
    text containing a backslash escape).
  - UC-2: window missing, containerized session — assert the mocked container runtime binary
    records a `run` invocation with the expected image/mounts, replacing the old
    Note-only assertion.
  - UC-2 alternative: mocked runtime/credential failure — assert a clear error and that no
    crash/panic occurs; window was still created.
  - UC-3: window present, agent pane reports the agent/container running — assert no
    `send-keys` or container `run` call happens, message unchanged from today.
  - UC-4: window present, agent pane reports a bare shell — assert relaunch happens with a
    message distinct from UC-3's.
  - UC-5: legacy record, no `agent` field, `cfg.agent` unset — today's message plus the new
    `Note:` pointing at `am run`.
  - UC-5 fallback: legacy record, no `agent` field, `cfg.agent` set — relaunches with the
    config's agent and persists it onto the record (assert via a follow-up `am list`/session
    read, not just stdout).
  - Regression: `am run <slug> <agent>` followed by `am attach` (window destroyed and
    recreated in between) relaunches the agent from `am run`, not whatever `cmd_start`
    originally recorded.
- [ ] Re-run full existing suite (`start.feature`, `tmux.feature`, `container.feature`,
      `destroy.feature`, `full_flow.feature`) to confirm no regressions to `am start`'s
      shared container-planning path after the extraction.

### code-reviewer

- [ ] Confirm no unverified per-agent resume flags were merged (OQ-3 spike must show its
      work — cite what was checked per agent, not just "guessed").
- [ ] Confirm the OQ-6 detection defaults to "do nothing" on any ambiguous
      `pane_current_command` read, never to "assume idle and relaunch."
- [ ] Confirm the shared container-planning extraction didn't change `cmd_start`'s existing
      behavior (same preflight order, same error messages) — a silent behavior drift here
      would be a `cmd_start` regression hiding inside an `am attach` PR.
- [ ] Confirm `Session.agent` and `AttachConfig` both round-trip through legacy records/config
      files with sensible defaults (no crash, no silent data loss).

### documentation-writer

- [ ] `docs/reference/commands.md` (`am attach` section, `:395-430`): rewrite to describe the
      relaunch/resume/container-recreate behavior, the `--fresh` flag, and the four distinct
      output messages; retire or reframe the current `Note:`/"restart cleanly" guidance as a
      manual fallback for when automatic recreate fails (A3), not the primary path.
- [ ] `docs/reference/configuration.md`: document the new `[attach]` section and `resume` key.
- [ ] `BACKLOG.md`: mark the attach-agent-restart gap resolved; add a one-line clarification
      next to the existing `postAttachCommand` entry (`:417-419`) that container recreation
      via this feature reruns create-time lifecycle hooks as needed (via `lifecycle_done`,
      unchanged) but is not the same as attaching to a live container — that follow-up stays
      open.

## Edge Cases & Considerations

- **Security:** no new attack surface — credential validation on the container-recreate path
  reuses `container::validate_agent_credentials`/`preflight_agent_auth`, exactly as `am
  start` already does. No new secrets are read, stored, or transmitted.
- **Performance:** UC-3's fast path adds exactly one tmux query
  (`pane_current_command`) — negligible. UC-2's container recreate is intentionally as slow
  as `am start`'s container path (image build if needed); this only triggers when the
  container is actually gone, so it doesn't regress the common case.
- **UX:** four distinct outcome messages (no-op / relaunched / container recreated /
  no-agent-known) so a user can tell at a glance what `am attach` actually did, especially
  the first time it silently would have "worked" by doing nothing under the old behavior.
- **Race conditions:** concurrent `am attach` for the same slug (A4) — pre-existing gap, not
  newly introduced, not fixed by this PRD; noted for a future hardening pass.
- **Agent CLI variability:** `am` cannot observe whether a resumed conversation actually
  resumed anything meaningful (OQ-5) — it is fire-and-forget via `send_keys`, identical to
  every other agent-launch path `am` already has.
- **`postAttachCommand` is still not implemented** (tracked separately in `BACKLOG.md`).
  Recreating a container here is not the same as exec-attaching into a live one; this feature
  does not close that gap, only the "container is actually gone" gap.
- **Process-name matching fragility (OQ-6, host path):** some agent CLIs run under a generic
  interpreter shim, so `pane_current_command` may not literally equal the agent name. Bias
  toward false negatives (no relaunch) over false positives (relaunch over a live agent) —
  see OQ-6's recommendation.

# Session lifecycle

This page explains what `am` actually creates when you start a session, how the three
independent systems — VCS worktree, tmux window, and container — interact, and what each
command does to them over the session's lifetime.

---

## The three systems

Every `am` session is made up of up to three pieces:

1. **VCS worktree or workspace** — always created. This is the agent's isolated working
   directory on a dedicated branch.
2. **tmux window** — created only when `am start` is run inside a tmux session.
3. **Container** — created only when container mode is enabled (the default) and `--no-container`
   is not passed.

All three are independent. You can run `am` without tmux, without containers, or without
either — every combination is valid. The session record in the global session store
(`$XDG_STATE_HOME/am/sessions.json`) reflects exactly what was created, along with the
repository the session belongs to.

---

## What `am start` creates

The combination of VCS type, container mode, and whether you are inside tmux determines what
`am start` sets up:

| VCS | Container | Inside tmux | What gets created |
|-----|-----------|-------------|-------------------|
| git | no  | no  | Worktree only; prints the worktree path |
| git | no  | yes | Worktree + split tmux window |
| git | yes | no  | Worktree; **execs into the container** (replaces current shell) |
| git | yes | yes | Worktree + container launched in the agent pane |
| jj  | no  | no  | Workspace only; prints the workspace path |
| jj  | no  | yes | Workspace + split tmux window |
| jj  | yes | no  | Workspace; **execs into the container** (replaces current shell) |
| jj  | yes | yes | Workspace + container launched in the agent pane |

"Container" means `container.enabled = true` in config (the default) and `--no-container`
was not passed to `am start`.

---

## Command behavior across combinations

### `am start`

All validation and early checks run before any side effects. An error at slug validation,
agent name validation, or missing container runtime leaves nothing behind.

The worktree (or workspace) is created first. If anything after that fails — tmux window setup,
container launch — the worktree may be left behind without a session record. Use
`am destroy --force <slug>` to clean up in that case.

### `am attach`

`am attach` is a tmux-only command. For sessions started without tmux (the first and third
rows of the table for each VCS), it exits with an error.

For sessions that have a tmux window, `am attach` handles four situations:

1. **Navigation** — the window exists and the agent (or, for a container session, the
   container) is confidently still running; `am attach` just switches your tmux focus to it.
2. **In-place relaunch** — the window exists but the agent pane is confidently idle (the agent
   exited, or the container's foreground process died, which also ends the container); `am
   attach` relaunches the recorded agent — or recreates the container — directly into the
   existing pane.
3. **Window recovery** — the window is gone entirely, most commonly because the machine
   rebooted; `am attach` recreates the window and split, then relaunches the agent (recreating
   the container first, for a containerized session) into the fresh agent pane.
4. **Deferred open** — you ran `am start` outside of tmux and later want a split window; `am
   attach` from inside tmux creates it the same way window recovery does.

By default, a relaunch also asks the agent to resume its previous conversation (`--continue`
for Claude/Copilot, `--resume latest` for Gemini, `resume --last` for Codex). Pass `--fresh`,
or set `resume = false` under `[attach]` in config, to start cold instead.

**Container sessions and window recovery:** containers run with `--rm -it`, so when a tmux
pane closes (killing the container process), the container stops and is automatically removed.
`am attach` detects this and recreates the container — re-running the same preflight `am start`
does (runtime detection, credential validation, and, in devcontainer mode, an image rebuild if
the previous image was pruned) — before handing the rebuilt run command to the fresh agent pane:

```
Opened new window for session 'feat' and restarted the container.
```

Because that preflight is real work, it can fail the same way `am start`'s can — a container
runtime that isn't running yet, a missing credential directory. If it does, the window and split
from step 3 above are still left in place (so you have something to retry against) and reported
as an error; fix the underlying problem and re-run `am attach` to pick up where it left off.

Note that preflight only checks that a credential *path* exists, not that the credential is
still valid — an expired token passes preflight, and `am attach` reports success while the agent
fails to authenticate inside the pane. See
[`am attach`](../reference/commands.md#am-attach-slug) for the full set of output messages.

### `am destroy`

Cleanup happens in this order, regardless of combination:

1. **Container** (best-effort) — `stop` then `rm`. Errors are ignored; with `--rm` the
   container may already be gone.
2. **Tmux** (best-effort) — kills the agent pane (or the whole window for old-style
   sessions). Errors are ignored; the window may not exist.
3. **Worktree** (required) — removes the git worktree or jj workspace and deletes the
   branch. This step fails hard unless `--force` is passed, so the session record is
   preserved and you can retry.

Use `--force` when the worktree is already gone but the session record remains, or when
you want to skip the confirmation prompt.

---

## The no-tmux + container path

When you run `am start` with container mode on but outside of tmux, `am` records the session
and then calls `exec()` to replace the current shell process with the container. From that
point you are working inside the container directly.

When you exit the container:
- The shell that ran `am start` returns (or the terminal closes, if exec was the top-level process)
- The container is stopped and auto-removed (because of `--rm`)
- The session record and worktree still exist on the host

Run `am destroy <slug>` from your host shell to remove the worktree and clean up the session
record. If the exec itself failed, the session was already recorded — use
`am destroy --force <slug>` to remove it without touching the (non-existent) worktree.

No tmux window was ever recorded for a session started this way, but `am attach <slug>` from
inside a tmux session still works — same as "Deferred open" above, it creates the window (and,
for a container session, relaunches the agent into a recreated container) on the spot.

---

## `am attach` vs `am start`

These two commands are not interchangeable:

- `am start <slug>` **creates** a new session. It errors if a session with that slug already exists.
- `am attach <slug>` **navigates** to an existing session. It errors if no session with that slug exists.

They complement each other: start to create, attach to return.

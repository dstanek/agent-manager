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

For sessions that have a tmux window, `am attach` handles three situations:

1. **Navigation** — the window exists; `am attach` switches your tmux focus to it.
2. **Window recovery** — the window was closed; `am attach` creates a new split window and
   shell pane for the session.
3. **Deferred open** — you ran `am start` outside of tmux and later want a split window;
   `am attach` from inside tmux creates it.

**Container sessions and window recovery:** containers run with `--rm -it`, so when a tmux
pane closes (killing the container process), the container stops and is automatically removed.
If you run `am attach` after the window has been closed, it creates a fresh empty agent pane
but the container is gone. `am attach` prints a hint in this case:

```
Opened new window for session 'feat'.
  Note: the container was stopped when the window closed.
  To restart cleanly: am destroy --force feat && am start feat
```

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

`am attach` is not available for sessions started this way, since there is no tmux window.

---

## `am attach` vs `am start`

These two commands are not interchangeable:

- `am start <slug>` **creates** a new session. It errors if a session with that slug already exists.
- `am attach <slug>` **navigates** to an existing session. It errors if no session with that slug exists.

They complement each other: start to create, attach to return.

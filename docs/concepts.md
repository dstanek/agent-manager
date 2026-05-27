# Concepts

This page explains the core ideas behind `am`. Understanding these will help you get the most out of the tool and make sense of the documentation.

---

## Sessions

A **session** is the central unit of work in `am`. When you run `am start`, a session is created that bundles together everything an agent needs to work independently:

- An isolated **branch** in your repository
- A **tmux window** with a split-pane layout
- An optional **container** wrapping the agent process

Sessions are tracked in `.am/sessions.json` and displayed by `am list`. When you're done, `am destroy` tears down all the pieces in one command.

The key idea is that a session is self-contained. Two sessions can run simultaneously in the same repository without stepping on each other, because each one has its own branch, its own terminal, and (optionally) its own container.

---

## Slugs

Every session is identified by a **slug** — a short name you choose when running `am start`. The slug flows through to everything `am` creates:

```
slug: fix-login
  → branch:    am/fix-login
  → worktree:  .am/worktrees/fix-login
  → tmux:      am-fix-login
  → container: am-fix-login
```

This naming convention makes it easy to find what belongs to a session no matter where you're looking — in `git branch`, `tmux list-windows`, or `podman ps`.

**Slug rules:** 1–40 characters, lowercase letters, digits, hyphens, and underscores only. Examples: `feat`, `fix-auth`, `refactor_api`.

---

## Worktrees and workspaces

`am` uses your VCS's built-in support for multiple simultaneous checkouts so the agent always works on a fresh, isolated branch:

- **git repos** — `am start` runs `git worktree add .am/worktrees/<slug> -b am/<slug>`, creating a new branch and a separate checkout directory. The agent's changes accumulate on `am/<slug>` without touching your current branch.
- **jj repos** — `am start` runs `jj workspace add .am/worktrees/<slug> --name <slug>`, creating an independent workspace. jj resolves the repo by walking up from the workspace directory.

In both cases, the checkout lives at `.am/worktrees/<slug>` and is removed when you run `am destroy`.

---

## The `.am/` directory

`am init` creates a `.am/` directory at the root of your repository:

```
.am/
├── config.toml       ← project-level am configuration
├── sessions.json     ← active session state (managed automatically)
└── worktrees/
    ├── feat/         ← worktree for the "feat" session
    └── fix-login/    ← worktree for the "fix-login" session
```

`.am/` is added to `.gitignore` automatically by `am init`. It is local to your machine — session state, worktrees, and project config are not committed to the repository.

---

## tmux

`am` uses **tmux** to give each session its own terminal environment. When you run `am start` inside a tmux session, `am` creates a new window named `am-<slug>` and splits it into two panes:

```
┌─────────────────────┬─────────────────────┐
│                     │                     │
│    Agent pane       │    Shell pane        │
│  (left, default)    │  (right, default)    │
│                     │                     │
│  $ claude           │  $ git diff         │
│  > working...       │                     │
│                     │                     │
└─────────────────────┴─────────────────────┘
           tmux window: am-feat
```

The **agent pane** is where `am` launches the container and the agent command. The **shell pane** is a plain shell in the worktree directory, ready for you to inspect files, run tests, or interact with the agent's output.

Use `am attach <slug>` to switch your tmux focus to an existing session's window from anywhere. The split direction, split ratio, and which pane gets the agent are all configurable — see [Configuration](reference/configuration.md#tmux).

If you run `am start` outside of tmux, the worktree and container are still created, but no window is opened. `am` prints the worktree path and the container run command so you can use them directly.

For a complete reference of how tmux, container, and VCS interact across all combinations, see [Session lifecycle](guides/session-lifecycle.md).

---

## Container isolation

By default, `am` runs each agent inside a **container** (Podman preferred, Docker as fallback). This puts a hard boundary around the agent process: it can only see what you explicitly mount into it, and it runs as an unprivileged user.

The container is launched interactively in the agent pane — you can see its output and interact with it directly.

**What gets mounted** into every container:

| Host path | Container path | Purpose |
|---|---|---|
| `.am/worktrees/<slug>` | same path as host | The agent's working directory |
| `<repo-root>/.git` (git) or `<repo-root>/.jj` (jj) | same path as host | VCS history and ref store |
| `~/.gitconfig` | `/home/am/.gitconfig` | Commit author identity |
| `~/.ssh` | `/home/am/.ssh` | SSH keys for remote operations |

Paths are mirrored from the host so that absolute paths inside the container resolve correctly. The container runs as the host user (matched uid/gid), so bind-mounted files are readable and writable without permission issues.

Container isolation can be disabled per-session with `--no-container`, or turned off by default in config with `container.enabled = false`.

Containers run with `--rm -it`: they are automatically removed when the tmux pane is closed or when the container's main process exits.

---

## Modes

`am` supports two modes of operation that reflect how much the agent drives the work versus how much you do.

### Interactive mode (default)

In **interactive mode** you are in the loop at all times. You start a session, the agent opens in its pane, and you direct the work through the agent's chat interface. The agent acts, you review, you steer.

This is the current default. Every `am start` invocation is interactive unless told otherwise.

```sh
am start feat --agent claude
```

### Autonomous mode *(coming soon — `--auto`)*

In **autonomous mode** you hand the agent a goal and step back. The agent works independently, making decisions without waiting for your input between steps. Useful for long-running or well-scoped tasks where you want to come back to a finished result.

```sh
am start feat --agent claude --auto   # future
```

### Team orchestration *(coming soon — `--team`)*

The `--team` flag will instruct `am` to start and coordinate multiple agents working toward a shared goal — each in its own isolated session, each on its own branch. `am` handles launching the sessions; the agents coordinate the work.

```sh
am start feat --team --agent claude   # future
```

---

## Agent integrations

`am` has built-in **agent integrations** for the most popular coding agents. Activating an integration (via `defaults.agent` in config or `--agent` on the command line) tells `am` to automatically provide that agent's credentials to the container at runtime, either by mounting host paths or by injecting required environment variables. The agent selection also determines the container image — `am` ships with built-in image defaults for `claude` and `copilot`, and you can configure images for any agent under `[agents.<name>]`.

| Agent | What gets mounted | Status |
|---|---|---|
| `claude` | `~/.claude` → `/home/am/.claude` (read-write) | ✓ Supported |
| `copilot` | `~/.config/gh` and `~/.config/github-copilot` → `/home/am/...` (read-only) | ✓ Supported |
| `gemini` | `~/.gemini` → `/home/am/.gemini` (read-only) | ✓ Supported |
| `codex` | _(env-var only — no filesystem mount)_ | ✓ Supported |

Credentials are **never baked into the container image** — they come from your host at session start. This means you can share a container image across machines or teammates without embedding any secrets.

The agent is also launched automatically inside the container after it starts, so you don't need to type the agent command yourself.

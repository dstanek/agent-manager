# Quick Start

Get from zero to your first agent session in five minutes.

---

## Before You Start

Make sure you have these prerequisites:

- **tmux** installed AND running (start with `tmux` if you haven't already)
- **Podman** or **Docker** installed and available on your `PATH`
- A **git repository** initialized (run `git init` if you need one)
- **An agent container image** (or a plan to build one — see below)

Don't have a container image yet? No problem. Follow the [Claude Code guide](../guides/claude-code.md) or [GitHub Copilot guide](../guides/github-copilot.md) to build one from a ready-to-use `Dockerfile`. Takes about 5 minutes.

See the [Installation guide](installation.md) for detailed setup instructions for each prerequisite.

---

## Sessions and slugs

Everything in `am` revolves around a **session** — a named, isolated workspace for a single agent working on a single task. A session bundles together:

- A dedicated **git branch** (`am/<slug>`) checked out as a worktree, so the agent's changes are completely separate from your main working tree
- A **tmux window** split into two panes: the agent on one side, your shell on the other
- An optional **container** wrapping the agent pane for hard process and filesystem isolation

You create and refer to sessions by their **slug** — a short, lowercase name you choose that describes the work. The slug becomes the branch name, the tmux window name, and the container name:

```
slug: feat
  → branch:    am/feat
  → window:    am-feat
  → container: am-feat
```

Slugs can contain lowercase letters, digits, hyphens, and underscores, and must be between 1 and 40 characters. Pick something descriptive: `feat`, `fix-login`, `refactor-api`.

---

---

## Step 1: Initialize your project

Navigate to your repository and run `am init`:

```sh
cd my-project
am init
```

This creates a `.am/` directory at the repository root containing:

- `.am/config.toml` — project configuration with all options commented out

`am init` also appends `.am/worktrees/` to your `.gitignore` so agent checkouts do not get committed. If `.gitignore` does not exist yet, it will be created. Note that `.am/config.toml` itself is *not* ignored — it is designed to be committed and shared with your team.

Session state is not stored in the repository at all. It lives in a per-user file at `$XDG_STATE_HOME/am/sessions.json` (falling back to `~/.local/state/am/sessions.json`), created on demand the first time you start a session.

---

## Step 2: Configure your agent

Open `.am/config.toml` and set the agent you want to use:

```toml
[defaults]
agent = "claude"
```

Setting the agent does two things: it activates the built-in credential mounts for that agent (e.g. `~/.claude` for `claude`) and automatically selects the right container image. `am` ships with built-in image defaults for `claude` and `copilot`.

To use a custom image, override it under `[agents.<name>]`:

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "ghcr.io/myorg/mydevimage:latest"
```

!!! tip "Need a ready-to-use image?"
    Pre-built images are available on the GitHub Container Registry: `ghcr.io/dstanek/am-claude:latest` and `ghcr.io/dstanek/am-copilot:latest`. See the [Claude Code guide](../guides/claude-code.md) or [GitHub Copilot guide](../guides/github-copilot.md) for full setup instructions, including how to build a custom image.

---

## Step 3: Start a session

Start your first session with a descriptive slug (the short name for this piece of work):

```sh
am start feat --agent claude
```

`am` performs the following steps automatically:

1. Creates a new `am/feat` branch as a git worktree at `.am/worktrees/feat`
2. Opens a new tmux window named `am-feat` with a 50/50 horizontal split
3. Launches the container in the left (agent) pane using the configured image
4. Waits briefly for the container to start, then sends the `claude` command to the agent pane
5. Keeps your shell available in the right pane

You are now looking at an isolated environment where the agent can make changes on its own branch without touching your main working tree.

---

## Step 4: Check your sessions

From any pane or terminal window, list your active sessions:

```sh
am list
```

Example output:

```
SLUG   AGENT    WINDOW     CREATED
feat   claude   am-feat    1 min ago
```

The table shows each session's slug, the agent running inside it, the tmux window name, and when it was created.

---

## Step 5: Work in parallel

One of the key benefits of `am` is running multiple agents simultaneously. Start a second session while the first is still active:

```sh
am start bugfix --agent claude
```

Each session has its own branch, its own tmux window, and its own container — they cannot interfere with each other:

```
SLUG     AGENT    WINDOW       CREATED
feat     claude   am-feat      5 min ago
bugfix   claude   am-bugfix    just now
```

Switch between sessions with `am attach`:

```sh
am attach feat
am attach bugfix
```

---

## Step 6: Destroy the session

When you are done with a session, destroy it:

```sh
am destroy feat
```

`am` will ask for confirmation before proceeding. To skip the prompt (useful in scripts or when you're confident):

```sh
am destroy feat --force
```

The destroy command:

1. Stops and removes the container
2. Kills the tmux window
3. Removes the git worktree and deletes the `am/feat` branch
4. Removes the session record from the global session store

---

## What's next?

- **Set up Claude Code** — follow the [Claude Code guide](../guides/claude-code.md) for a complete container image and configuration walkthrough
- **Explore all options** — see the [Configuration reference](../reference/configuration.md) to customize tmux layout, container settings, and more
- **Learn all commands** — the [Commands reference](../reference/commands.md) documents every `am` subcommand and its flags

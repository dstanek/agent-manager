# Quick Start

Get from zero to your first agent session in five minutes.

---

## Before You Start

You need `am` installed and a **git or jj repository** to run it in (`git init` if you don't have one yet). Everything else — tmux, a container runtime, your agent's credentials — is worth having, but you don't need to check for it by hand: [`am setup`](#step-1-set-up-your-project) below runs the same checks as `am doctor` and tells you exactly what's missing and how to fix it.

If you'd rather line everything up first:

- **tmux** installed AND running (start with `tmux` if you haven't already)
- **Podman** or **Docker** installed and available on your `PATH`
- **An agent container image** (or a plan to build one — see below)

Don't have a container image yet? No problem. `am` ships with built-in images for `claude` and `copilot` on the GitHub Container Registry, so you can start without building anything. To use your own, follow the [Claude Code guide](../guides/claude-code.md) or [GitHub Copilot guide](../guides/github-copilot.md) to build one from a ready-to-use `Dockerfile`. Takes about 5 minutes.

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

## Step 1: Set up your project

Navigate to your repository and run the guided setup:

```sh
cd my-project
am setup
```

`am setup` is the on-ramp: it does the same directory/`.gitignore` work `am init` does (see the [tip](#already-know-what-you-want) below), then asks only the questions it can't answer by detecting your machine — which agent you use, and whether you want isolated containers — verifies the result with the same checks `am doctor` runs, asks how you'd like your tmux panes laid out, and can start your first session for you. A run on a brand-new repo looks like this:

```
Setting up am for the git repository at /home/user/my-project
  Created .am/config.toml
  Added .am/worktrees/ to .gitignore
  Created ~/.config/am/config.toml

Which agent do you use?
  just this repo; saved to .am/config.toml.

  [1] claude    credentials found
  [2] copilot
  [3] gemini
  [4] codex

  currently: none configured

Agent [1-4] (Enter for claude): 

Use isolated containers for your sessions?
  every repo on this machine; saved to ~/.config/am/config.toml.

Each session gets its own isolated filesystem and process sandbox. Without containers, sessions run directly on the host.

Use isolated containers for your sessions? [Y/n] 

Checking your setup...

[... the same report 'am doctor' prints ...]
Ready.

Which layout do you want?
  every repo on this machine; saved to ~/.config/am/config.toml.

  [1] agent left, 50/50
      [  agent   |  shell   ]
  [2] agent right, 50/50
      [  shell   |  agent   ]
  [3] agent left, 70/30
      [    agent    | shell ]
  [4] stacked, agent on top, 50/50
      [       agent        ]
      [       shell        ]
  [5] customize…

  currently: agent_pane=left, split=horizontal, split_percent=50 (am's default)

Layout [1-5] (Enter to keep current): 

Start your first session now? [Y/n] y
Session name: feat
```

Press Enter to accept the shown default at any prompt — an agent already detected on your host, containers enabled, `am`'s default pane layout. Setting the agent activates its built-in credential mounts (e.g. `~/.claude` for `claude`) and selects the right container image; `am` ships with built-in image defaults for `claude` and `copilot`, so you don't need to configure one yourself unless you want a custom image (see below).

If `am setup` finds something that would stop `am start` from working — no container runtime installed, an agent that isn't authenticated — it stops right there and tells you exactly what to do:

```
2 problems will prevent 'am start' from working.

What to do next:
  - install Podman (https://podman.io/docs/installation) or Docker (https://docs.docker.com/get-docker/), or set container.enabled = false in .am/config.toml
  - run 'claude auth login' (or set ANTHROPIC_API_KEY) — see docs/guides/claude-code.md#prerequisites

Then re-run 'am setup'.
```

Fix what it lists and re-run `am setup` — it remembers what you already answered and only asks again if that answer's file no longer has it.

To use a custom image, add it under `[agents.<name>]` in `.am/config.toml` by hand — `am setup` doesn't ask about images:

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "ghcr.io/myorg/mydevimage:latest"
```

!!! tip "Already know what you want?"
    `am init` does the same directory/`.gitignore` setup with no questions asked — the fast, scriptable path:

    ```sh
    am init
    ```

    It creates `.am/config.toml` with every option commented out and appends `.am/worktrees/` to `.gitignore` (creating it if needed). Note that `.am/config.toml` itself is *not* ignored — it is designed to be committed and shared with your team. Open it and set the agent yourself:

    ```toml
    [defaults]
    agent = "claude"
    ```

    See the [Commands reference](../reference/commands.md#am-init) and the [Claude Code guide](../guides/claude-code.md) or [GitHub Copilot guide](../guides/github-copilot.md) for full setup instructions, including how to build a custom image.

Session state is not stored in the repository at all. It lives in a per-user file at `$XDG_STATE_HOME/am/sessions.json` (falling back to `~/.local/state/am/sessions.json`), created on demand the first time you start a session.

---

## Step 2: Start a session

If you accepted `am setup`'s offer to start a first session above, you already have one running — skip ahead to [Step 3](#step-3-check-your-sessions).

Otherwise, start one now with a descriptive slug (the short name for this piece of work):

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

## Step 3: Check your sessions

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

## Step 4: Work in parallel

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

## Step 5: Destroy the session

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

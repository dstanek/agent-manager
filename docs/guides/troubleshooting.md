# Troubleshooting

This guide covers common issues when using `am` and how to resolve them. Issues are organized by category for quick reference.

---

## Authentication Issues

### Claude Code is not authenticated

**Agent:** Claude

If you see an authentication error when the container starts:

1. Run `claude auth login` on your host machine to create the `~/.claude/` credential directory
2. Restart the session:

```sh
am destroy feat --force
am start feat --agent claude
```

The `~/.claude/` directory must exist on your host before starting a session. `am` mounts this directory read-only into the container at runtime.

### Passing `ANTHROPIC_API_KEY` directly

**Agent:** Claude

If you prefer to authenticate via API key rather than the `~/.claude` credential store:

1. Set your API key in the environment on your host:

```sh
export ANTHROPIC_API_KEY="sk-ant-..."
```

2. Add it to the container's environment in `.am/config.toml`:

```toml
[defaults]
agent = "claude"

[container]
env = ["ANTHROPIC_API_KEY"]
```

The API key is passed from your shell environment into the container and never stored on disk inside the container.

### GitHub Copilot authentication not working in the container

**Agent:** GitHub Copilot

If Copilot reports authentication errors when the session starts:

1. Check your host authentication:

```sh
gh auth status
gh copilot --version
```

2. Re-authenticate if needed:

```sh
gh auth login
```

3. Restart the session:

```sh
am destroy feat --force
am start feat --agent copilot
```

### Two credential directories required for Copilot

**Agent:** GitHub Copilot

Copilot requires **both** of these directories on your host:

- `~/.config/gh/` — GitHub CLI OAuth token
- `~/.config/github-copilot/` — Copilot-specific settings and cached tokens

If either directory is missing, the agent will fail to authenticate. Both are created automatically when you run `gh auth login` on a machine with Copilot access.

### Gemini API key not found

**Agent:** Gemini

If Gemini reports an authentication error:

1. Verify your API key is set:

```sh
echo $GOOGLE_API_KEY
```

2. If not set, obtain a free API key from [Google AI Studio](https://aistudio.google.com/app/apikey) and add it to your environment:

```sh
export GOOGLE_API_KEY="..."
```

3. Create the `~/.gemini/` directory if it doesn't exist:

```sh
mkdir -p ~/.gemini
```

4. Restart the session:

```sh
am destroy feat --force
am start feat --agent gemini
```

### Codex authentication fails in the container

**Agent:** Codex

Codex accepts either an API key or an interactive sign-in, and `am` supports both. You need only one.

**If you signed in interactively** (`codex` on your host, no API key), the credentials live in `~/.codex/auth.json`. `am` mounts the whole `~/.codex` directory read-write into the session, so sign-in carries over and token refreshes are written back. Check that the directory exists on the host:

```sh
ls ~/.codex/auth.json
```

If it is missing, run `codex` once on the host and complete the sign-in.

!!! warning "Shared state across sessions"

    Because the whole directory is mounted, your host codex and every concurrent `am` session share the same history and SQLite databases under `~/.codex`. Running several codex sessions at once may interleave their history.

**If you use an API key:**

1. Ensure `OPENAI_API_KEY` is set in your host shell:

```sh
export OPENAI_API_KEY="sk-..."
```

2. Verify the key is available inside the container:

```sh
echo $OPENAI_API_KEY
```

3. To persist the API key across sessions, add it to your shell configuration (`.bashrc`, `.zshrc`, etc.) or configure it in `.am/config.toml`:

```toml
[defaults]
agent = "codex"

[container]
env = ["OPENAI_API_KEY"]
```

### Aider API key not found in the container

**Agent:** Aider

If Aider reports an authentication error:

1. Ensure your API key environment variable is set in your host shell:

```sh
# For Claude/Anthropic
export ANTHROPIC_API_KEY="sk-ant-..."

# Or for OpenAI
export OPENAI_API_KEY="sk-..."
```

2. Verify the key is available inside the container:

```sh
echo $ANTHROPIC_API_KEY  # or OPENAI_API_KEY
```

3. To persist the API key across sessions, add it to your shell configuration or configure it in `.am/config.toml`:

```toml
[defaults]
agent = "codex"

[container]
env = ["ANTHROPIC_API_KEY"]
```

---

## Prerequisites & Setup

### Missing `~/.claude/` directory

**Agent:** Claude

Before using Claude Code, run `claude auth login` on your host machine:

```sh
claude auth login
```

This creates the `~/.claude/` directory where `am` expects to find credentials. Verify it exists:

```sh
ls -la ~/.claude/
```

### Missing tmux

**Applies to:** All agents

`am` requires `tmux` to be installed and running. To start or attach to a tmux session:

```sh
tmux
```

If `tmux` is not installed, install it with your package manager:

```sh
# macOS
brew install tmux

# Ubuntu/Debian
sudo apt-get install tmux

# Fedora/RHEL
sudo dnf install tmux
```

### Commands require running inside tmux

**Applies to:** `am attach`

These commands must be run from inside a tmux session. If you see an error about `$TMUX` not being set:

1. Start or attach to tmux:

```sh
tmux
```

2. Then run your `am` command again.

**Alternative:** Navigate directly to the worktree without tmux:

```sh
cd .am/worktrees/<slug>
```

### `am init` must be run in a git or jj repository

**Applies to:** `am init`

`am` requires a git or jj repository to initialize. If you see an error:

1. Initialize git if needed:

```sh
git init
```

2. Then run `am init`:

```sh
am init
```

!!! note
    `am` checks for `.jj/` first; if not found, it checks for `.git/`.

---

## Container & Image Issues

### Customizing the container image

**Agents:** Claude, Copilot, Gemini, Codex, Aider

To add project-specific language runtimes or tools:

1. Start with the minimal Dockerfile as a base:

- Claude: `dockerfiles/Dockerfile.claude-minimal`
- Copilot: `dockerfiles/Dockerfile.copilot-minimal`
- Gemini: `dockerfiles/Dockerfile.gemini-minimal`
- Codex: `dockerfiles/Dockerfile.codex-minimal`

2. Layer your tools on top and build:

```dockerfile
FROM ghcr.io/dstanek/am-claude-minimal:latest

# Add your project-specific tools
RUN apt-get update && apt-get install -y \
    your-tools-here
```

3. Configure in `.am/config.toml`:

```toml
[agents.claude]
image = "ghcr.io/myorg/mydevimage:latest"
```

The only hard requirement is that the agent binary (e.g., `claude`, `gh`, `gemini`) is on the container's `PATH`. See the [Custom Container Images](custom-images.md) guide for detailed patterns and examples.

### Pinning to a specific version for reproducibility

**Agents:** Claude, Copilot, Gemini, Codex, Aider

To ensure reproducible environments, pin to a specific release tag instead of `:latest`:

```sh
# Instead of :latest
ghcr.io/dstanek/am-claude:0.2.0
ghcr.io/dstanek/am-copilot:0.2.0
ghcr.io/dstanek/am-gemini:0.2.0
```

Check the [releases page](https://github.com/dstanek/am/releases) for available tags.

### Container image selection not working

**Applies to:** All agents

If `am` is not selecting the correct image:

1. Override the default in `.am/config.toml`:

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "ghcr.io/myorg/myimage:latest"
```

2. Verify the configuration is correct:

```sh
am generate-config
```

3. Restart the session:

```sh
am destroy feat --force
am start feat --agent claude
```

---

## Command-Specific Issues

### Invalid slug format

**Command:** `am start <slug>`

Slugs must match: `[a-z0-9_-]{1,40}` (1–40 characters, lowercase letters, digits, hyphens, underscores only).

**Valid examples:**

```
am start feat
am start fix-auth
am start my_feature
am start v2
am start release-2026-03
```

**Invalid examples:**

```
am start MyFeature          # uppercase not allowed
am start "fix auth"         # spaces not allowed
am start -leading-dash      # must start with letter or digit
am start feature-name-that-is-longer-than-40-chars  # too long
```

### `am init` is safe to run multiple times

**Command:** `am init`

Running `am init` multiple times in the same directory is safe. An existing `.am/config.toml` is not overwritten, and `.gitignore` is only updated if needed. `am init` does not touch session state, which lives outside the repository entirely.

### No sessions active

**Command:** `am list`

If no sessions are currently active, `am list` prints a friendly message. Create a new session with `am start <slug>`.

### Tmux window doesn't exist after system restart

**Command:** `am attach <slug>`

If a tmux window is not found, `am attach` will automatically create a new window and split. You may need to restart the container separately by running:

```sh
am destroy feat --force
am start feat --agent claude
```

---

## Mount Points & Git Configuration

### Git commands not working inside the container

**Agents:** Claude, Copilot, and all agents with git repositories

`am` automatically mounts and configures git for your agent:

| Host path | Container path | Mode |
|---|---|---|
| `.am/worktrees/<slug>` | same path as host | read-write |
| `<repo-root>/.git` | same path as host | read-write |
| `~/.gitconfig` | `/home/am/.gitconfig` | read-only |
| `~/.ssh` | `/home/am/.ssh` | read-only |

Paths are mirrored from the host so that absolute paths resolve correctly inside the container. The container runs as the host user (matched uid/gid).

!!! note "jj repositories"
    For jj repositories, the `.jj` directory is mounted at the same host path instead of `.git`. Colocated jj+git repos mount both.

### SSH and Git credentials inside the container

Your SSH keys from `~/.ssh` are mounted read-only into the container at `/home/am/.ssh`. Similarly, `~/.gitconfig` is mounted at `/home/am/.gitconfig`. This allows the agent to make authenticated git and SSH operations (e.g., `git push`, `git pull` from private repositories).

---

## Session Lifecycle

### Stopping and removing a session

**Command:** `am destroy <slug>`

To permanently stop and remove a session:

```sh
am destroy feat --force
```

!!! warning
    `am destroy` is destructive:
    - ✓ Stops and removes the container
    - ✓ Kills the tmux window
    - ✓ Removes git worktree and deletes the `am/<slug>` branch
    - ✓ Removes the session from the global session store
    - ✗ These changes cannot be undone
    - ✗ Worktree and branch are permanently deleted

Without `--force`, `am` will prompt for confirmation before destruction.

---

## General Tips

### Verify configuration

To see how `am` is interpreting your configuration (CLI flags, environment variables, `.am/config.toml`, and defaults merged together):

```sh
am generate-config
```

This shows the resolved configuration without making any changes.

### Check session status

List all active sessions and their details:

```sh
am list
```

### Run a session non-interactively

`am` has no "exec into a running session" command. For a non-interactive run from the start, use
a containerized session started outside tmux: `am start` records the session, then `exec`s the
container in place of the `am` process itself, so it runs to completion and exits with the
container's real exit code — no split pane, no second shell.

---

## Still Not Working?

If you encounter an issue not covered here:

1. Check the agent-specific guide:
   - [Claude Code](claude-code.md)
   - [GitHub Copilot](github-copilot.md)
   - [Gemini](gemini.md)
   - [Codex](codex.md)

2. Review your configuration with `am generate-config` to verify settings are correct.

3. Check container logs:

```sh
docker ps  # Find the container ID
docker logs <container-id>
```

4. Open an issue on [GitHub](https://github.com/dstanek/am/issues) with your error message and configuration.

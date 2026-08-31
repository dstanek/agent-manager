# Quick Reference

One-page guide to common `am` commands, workflows, and configurations.

---

## Commands at a Glance

| Command | Purpose |
|---------|---------|
| `am init` | Initialize am in current repo |
| `am doctor` | Report what is/isn't ready for `am start` |
| `am start <slug>` | Create a new agent session |
| `am list` | Show active sessions in the current repo |
| `am list --all` | Show active sessions across every repo |
| `am attach <slug>` | Switch to existing session |
| `am destroy <slug>` | Stop and remove session |
| `am session rm <slug>` | Remove a session record, leaving worktree and branch alone |
| `am generate-config` | Show resolved configuration |

---

## Common Workflows

### Start Your First Session

```bash
# Initialize once per repository
am init

# Create session with Claude Code
am start feature --agent claude

# In the agent pane, Claude Code opens automatically
# In the shell pane, you can run git commands, tests, etc.
```

### Work with Multiple Agents in Parallel

```bash
# Terminal 1: Start Claude for feature development
am start feature --agent claude

# Terminal 2 (different window): Start Copilot for tests
am start tests --agent copilot

# Terminal 3: Check status
am list

# Both agents work independently on separate branches
```

### Run a Session Non-Interactively

```bash
# Useful for scripts. Outside tmux, a containerized `am start` records the
# session, then execs the container in place of the `am` process itself —
# runs to completion, exits with the container's real exit code.
am start task --agent claude
```

### Destroy All Sessions (Cleanup)

```bash
# Destroy one session
am destroy feature --force

# Destroy multiple (by repeating)
am destroy feature --force
am destroy tests --force
am destroy docs --force
```

---

## Configuration Snippets

### Set Default Agent

Create or edit `.am/config.toml`:

```toml
[defaults]
agent = "claude"
```

Then `am start feat` automatically uses Claude.

### Use Custom Container Image

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "ghcr.io/myorg/my-claude:latest"
```

### Define a Custom Agent

`--agent` isn't limited to the four built-ins (`claude`, `copilot`, `gemini`, `codex`) — add a
`[agents.<name>]` section for any other CLI and use it the same way:

```toml
[agents.aider]
command = ["aider", "--model", "sonnet"]

[agents.aider.integration]
env = ["ANTHROPIC_API_KEY"]
requires_any = [[{ env = "ANTHROPIC_API_KEY" }]]
```

```bash
am start feat --agent aider
```

See the [custom agents guide](../guides/custom-agents.md) and the
[configuration reference](configuration.md#custom-agents) for the full shape.

### Adjust Tmux Layout

```toml
[tmux]
split = "vertical"              # or "horizontal"
agent_pane = "left"             # or "right"
split_percent = 40              # agent pane gets 40%, shell gets 60%
```

### Disable Container (Run Directly in Shell)

```toml
[container]
enabled = false
```

Or at runtime:

```bash
am start feat --no-container
```

### Pass Environment Variables to Container

```toml
[container]
env = ["ANTHROPIC_API_KEY", "MY_VAR=custom_value"]
```

The first form passes the host's environment variable through; the second sets a literal value.

---

## Slug Validation

Valid slugs: **1–40 characters**, lowercase letters (a–z), digits (0–9), hyphens (-), underscores (_)

| ✅ Valid | ❌ Invalid |
|----------|------------|
| `feat` | `MyFeature` (uppercase) |
| `fix-auth` | `fix auth` (space) |
| `v2` | `-leading` (starts with dash) |
| `release-2026-03` | `feature-way-too-long-name-over-forty-characters` (>40 chars) |
| `my_feature_v3` | `feat@branch` (special character) |

---

## Key Concepts

| Term | Definition |
|------|-----------|
| **Session** | Self-contained agent environment (branch, tmux window, container) |
| **Slug** | Session name used in all commands; becomes branch name, window name, etc. |
| **Worktree** | Separate git checkout at `.am/worktrees/<slug>` |
| **Agent Pane** | Left/right side of split tmux window where agent runs |
| **Shell Pane** | Other side of split window for your shell commands |
| **Integration** | An agent's credential wiring — mounts, env vars, and how to check it's authenticated. Built in for Claude, Copilot, Gemini, and Codex; definable for any other agent under `[agents.<name>].integration` |

---

## Directory Structure

```
your-repo/
├── .am/
│   ├── config.toml          ← Project config (edit this; commit it)
│   └── worktrees/           ← gitignored
│       ├── feat/            ← Worktree for 'feat' session
│       ├── tests/           ← Worktree for 'tests' session
│       └── docs/            ← Worktree for 'docs' session
├── .git/
└── ... (your files)

$XDG_STATE_HOME/am/          ← per-user, outside any repo
├── sessions.json            ← Active sessions, all repos (auto-managed)
└── gitconfig                ← Identity mounted into containers (auto-generated)
```

---

## Tmux Cheat Sheet (Inside a Session)

Once inside an `am` session, you're in tmux. Here are essential shortcuts:

| Shortcut | Action |
|----------|--------|
| `Ctrl-b` `%` | Split pane vertically (tmux default) |
| `Ctrl-b` `"` | Split pane horizontally (tmux default) |
| `Ctrl-b` `←/→` | Move between panes (arrow keys) |
| `Ctrl-b` `[` | Enter scroll/copy mode (press `q` to exit) |
| `Ctrl-b` `c` | Create new window in this session |
| `Ctrl-b` `n` | Switch to next window |
| `Ctrl-b` `d` | Detach from session (leaves it running) |

---

## Environment Variables

Useful `AM_*` variables for overriding config without editing files:

```bash
# Set default agent for this invocation
AM_AGENT=claude am start feat

# Disable container
AM_CONTAINER_ENABLED=false am start feat

# Use Docker instead of Podman
AM_CONTAINER_RUNTIME=docker am start feat

# Use Copilot and vertical split
AM_AGENT=copilot AM_TMUX_SPLIT=vertical am start feat

# Use no network inside the container
AM_CONTAINER_NETWORK=none am start feat

# Override which git config to mount
AM_CONTAINER_GITCONFIG=~/.my-gitconfig am start feat
```

For a complete list, see [Configuration Reference](configuration.md#environment-variables).

---

## File Locations

| Path | Purpose |
|------|---------|
| `.am/config.toml` | Project-level configuration (committable) |
| `.am/worktrees/<slug>/` | Agent's working directory |
| `$XDG_STATE_HOME/am/sessions.json` | Track active sessions across all repos (`~/.local/state/am/` by default) |
| `$XDG_STATE_HOME/am/gitconfig` | Generated identity mounted into containers |
| `~/.config/am/config.toml` | Global config (all projects) |
| `~/.claude/` | Claude Code credentials |
| `~/.config/gh/` | GitHub CLI auth (for Copilot) |
| `~/.config/github-copilot/` | Copilot settings |

---

## Common Errors & Quick Fixes

| Error | Cause | Fix |
|-------|-------|-----|
| `not a git or jj repo` | Not in a repository | `cd` to repo root |
| `slug already exists` | Session name taken | Use different slug |
| `$TMUX not set` | Running outside tmux | Run `tmux` first or use `--no-container` |
| `container failed to start` | Image not found or won't pull | Check `am generate-config` or pull image manually |
| `authentication error` | Credentials not set up | Run `claude auth login` (or `gh auth login` for Copilot) |
| `permission denied` | Container user permission issue | Usually a mount path problem; check troubleshooting guide |

See [Troubleshooting Guide](../guides/troubleshooting.md) for detailed solutions.

---

## Getting Help

| Need | Command |
|------|---------|
| See all options | `am --help` |
| Help for specific command | `am start --help` |
| Validate config | `am generate-config` |
| Check active sessions | `am list` |
| Clear a `stale` session row | `am session rm <slug> --repo <path>` |
| Read full docs | Visit [documentation home](../index.md) |

---

## Copy-Paste Templates

### Minimal Config

```toml
[defaults]
agent = "claude"
```

### Full Config (All Options)

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "ghcr.io/dstanek/am-claude:latest"

[tmux]
agent_pane = "left"
split = "horizontal"
split_percent = 50

[container]
enabled = true
runtime = "auto"
network = "full"
env = []
```

### CI/CD Integration

Outside tmux, a containerized `am start` records the session, then `exec`s the container in
place of the `am` process itself, so it runs to completion with a real exit code a pipeline can
check. There's no verified, runner-tested worked example yet — see
[FAQ: Can I run `am` in a CI/CD pipeline?](../guides/faq.md#can-i-run-am-in-a-cicd-pipeline).

---

## Performance Tips

| Tip | Benefit |
|-----|---------|
| Pre-pull image: `docker pull ghcr.io/dstanek/am-claude:latest` | Faster session startup |
| Use `--no-container` for debugging | Faster iteration (no container overhead) |
| Configure `split_percent` to your preference | Better use of screen space |

---

## Next Steps

- **Getting started?** → Read [Installation](../getting-started/installation.md)
- **More detail?** → See [Commands Reference](commands.md)
- **Troubleshooting?** → Check [Troubleshooting Guide](../guides/troubleshooting.md)
- **Advanced topics?** → Explore [Custom Images](../guides/custom-images.md) and [Dev Containers](../guides/devcontainers.md)

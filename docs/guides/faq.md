# FAQ & Gotchas

Frequently asked questions and common misconceptions about `am`.

---

## General Questions

### What exactly is `am`?

`am` (Agent Manager) is a CLI tool that creates **isolated environments** for AI coding agents like Claude Code and GitHub Copilot. Each agent gets its own git branch, tmux window, and optional container — so multiple agents can work in parallel without interfering with each other.

Think of it as **tmux + git worktrees + containers** all orchestrated for AI agents.

### When should I use `am`?

**Use `am` when:**
- You want to run multiple agents in parallel (feature + tests + docs)
- You want agent work isolated from your main branch
- You prefer containers for security/consistency
- You're prototyping features with AI assistance
- You want to experiment without polluting your working directory

**Don't use `am` if:**
- You only ever work with one branch at a time (use git directly)
- You strongly prefer no containers (though `--no-container` exists)
- You need real-time collaboration (multiple humans on same code)

### Can I use `am` without tmux?

Partially. `am` **requires** tmux for its split-pane interface (agent on one side, shell on the other). However:

- **If inside tmux:** Everything works normally
- **If outside tmux:** Running `am start` without tmux will execute `am` but replace your shell process with the container. No split pane, no second shell.
- **Alternative:** Use `am run <slug> -- <cmd>` to execute commands without tmux at all

For non-interactive use (scripts, CI/CD), you don't need tmux.

### Do I need Docker or Podman?

No, but it's **strongly recommended**. Containers provide:
- Isolation (agent can't see/modify your system)
- Reproducibility (same image works everywhere)
- Portability (easy to share)

Use `--no-container` to run directly in your shell, but you lose these benefits.

`am` defaults to Podman (preferred), falls back to Docker, or you can choose with `AM_CONTAINER_RUNTIME`.

---

## Git & Branching

### Why don't my changes appear in the main branch?

**By design!** Each session runs on its own `am/<slug>` branch:

```
main
├─ am/feature
├─ am/tests
└─ am/docs
```

Changes stay on the session's branch until you explicitly merge them. This is intentional isolation.

**To merge:** `git merge am/feature` or use a pull request.

### What happens when I delete `.am/worktrees/<slug>`?

Don't do this — use `am destroy <slug>` instead.

If you delete it manually:
- The branch `am/<slug>` still exists in git (orphaned)
- The session is still recorded in the global session store
- `am` may get confused

**Fix:** Use `am destroy <slug>` to properly clean up. If the repository itself is gone — so there is nothing left for `am destroy` to work on — use `am session rm <slug> --repo <path>` to drop the leftover record instead.

### Can I manually edit the `.am/<slug>` branch?

Yes! It's a normal git branch. You can:
- Push it to GitHub as a PR
- Cherry-pick commits from it
- Rebase it
- Merge it like any other branch

`am` doesn't lock or protect the branch — it's yours to modify.

### Can multiple users share `.am/`?

Partly, and the useful half is the intended workflow.

`.am/config.toml` is meant to be committed and shared. Session state is no longer kept in the repository — each user has their own store at `$XDG_STATE_HOME/am/sessions.json` — so a shared config file no longer drags a shared session registry along with it. Committing `.am/config.toml` gives your whole team the same `agent`, `container`, and `devcontainer` defaults.

What is still not supported is two people working out of the *same checkout* at the same time: `.am/worktrees/` is a single directory on disk, and two sessions with the same slug would collide there. Each user should have their own clone.

---

## Containers & Images

### Do credentials get stored in the container image?

**No.** Credentials are mounted at **runtime** from your host:
- `.claude/` → mounted read-only
- `.config/gh/` → mounted read-only
- `OPENAI_API_KEY` → passed as env var

The image itself contains zero secrets. You can safely share or publish images.

### Can I use my own container image?

Yes! Configure in `.am/config.toml`:

```toml
[agents.claude]
image = "ghcr.io/myorg/my-claude:v1"
```

Or `[container] image = "..."` to override for all agents.

Your image only needs the agent binary on `PATH`. See [Custom Container Images](../guides/custom-images.md) guide.

### Why is my image so large?

Pre-built images (`ghcr.io/dstanek/am-*`) include:
- Base OS (Ubuntu)
- Git + jj
- Agent binary (Claude, etc.)
- Common tools (curl, ripgrep, jq, neovim)

**To shrink:** Use `-minimal` variants or build your own minimal image. The minimal images include only git and the agent binary.

### Does `am` work with arm64 / M1 / M2 Macs?

Yes! Images are published for both `amd64` and `arm64`. Docker/Podman automatically pulls the right one.

If you build locally on an M1 Mac, the image will be `arm64` by default.

---

## Configuration & Setup

### What's the difference between `.am/config.toml` and `~/.config/am/config.toml`?

**Project config** (`.am/config.toml`):
- Per-repository settings
- Committed to git (shared with team)
- Overrides global settings

**Global config** (`~/.config/am/config.toml`):
- Machine-wide defaults
- Not committed to git
- Fallback for all projects

**Precedence:** CLI flags > env vars > project config > global config > defaults

### Can I use `am` without any config?

Yes! `am` ships with sensible defaults:
- Agent: none (you specify at `am start`)
- VCS: git (auto-detect between git/jj)
- Container: enabled (Podman preferred, Docker fallback)

Just run `am init` and you're ready: `am start feat --agent claude`

### How do I know what config is actually being used?

Run `am generate-config` — it shows the resolved configuration from all layers merged together.

### Why am I getting "permission denied" errors in the container?

Likely causes:
- **SSH directory permissions:** `chmod 600 ~/.ssh/*` on host
- **Home directory:** Container runs as user `am` (UID 1000), may have permission issues
- **SELinux:** If enabled, containers may not mount directories

**Fix:** Check [Troubleshooting Guide](../guides/troubleshooting.md) for detailed solutions.

---

## Workflows & Common Tasks

### How do I run a command inside a session without attaching?

Use `am run`:

```bash
am run feat -- your-command-here
```

This executes the command in the session's container and prints output. Useful for CI/CD and scripts.

### Can I keep a session running while I close my terminal?

Yes! `am` sessions run in tmux, which is a persistent terminal multiplexer.

**To detach:** Inside tmux, press `Ctrl-b` `d` (detach)
**To reconnect:** `am attach feat`

The session keeps running in the background.

### How do I pause/resume a session?

Sessions don't pause — they run continuously. You can:
- **Detach:** `Ctrl-b` `d` (leaves it running)
- **Attach:** `am attach feat` (rejoin)
- **Stop:** `am destroy feat` (terminates)

### Why does my agent take a long time to start?

Causes:
1. **Container image pulling** (first time or after update)
2. **Slow machine** or low disk I/O
3. **Agent startup time** (Claude, Copilot both have startup overhead)

**Fixes:**
- Pre-pull the image: `docker pull ghcr.io/dstanek/am-claude:latest`
- Use `-minimal` images (smaller, faster)

---

## Advanced Questions

### Can I use `am` with GitHub Pages / static sites?

Yes, but you need to decide what branches to publish. `am` creates `am/<slug>` branches, not your `main` branch.

**Typical flow:**
1. `am start feature --agent claude`
2. Agent works on `am/feature` branch
3. You review and `git merge am/feature` to `main`
4. GitHub Pages publishes from `main`

The agent never directly publishes.

### Can `am` work with monorepos?

Yes! Each session gets its own worktree, so agents can work on different parts of the monorepo independently:

```bash
am start feature-backend --agent claude
am start feature-frontend --agent claude
```

Both work in parallel on their own branches (`am/feature-backend`, `am/feature-frontend`).

### Does `am` work with other VCS systems (Mercurial, Fossil)?

Currently only git and jj are supported. Support for other VCS would require:
- Detecting the VCS type
- Implementing worktree/workspace operations
- Handling the VCS-specific branch naming

This is a feature request, not currently implemented.

### Can I run `am` in a CI/CD pipeline?

Yes! See [CI/CD Integration Guide](../guides/ci-cd-integration.md) for examples:
- GitHub Actions
- GitLab CI
- Jenkins

Key insight: Use `am run` instead of `am attach` in non-interactive environments.

### Does `am` store any telemetry or usage data?

No. `am` is local-only. It doesn't phone home, send usage stats, or require a server. Everything stays on your machine.

---

## Troubleshooting

### How do I debug what `am` is doing?

1. **Check config:** `am generate-config`
2. **Check sessions:** `am list`
3. **Check container logs:** `docker logs <container-id>` or `podman logs <container-id>`
4. **Check tmux status:** `tmux list-windows`
5. **Read troubleshooting guide:** [Troubleshooting Guide](../guides/troubleshooting.md)

### What if `am` crashes or behaves unexpectedly?

Clean up and try again:

```bash
# Destroy the session
am destroy feat --force

# Check for orphaned tmux windows
tmux list-windows

# Check for orphaned containers
docker ps -a | grep am-feat
docker rm <container-id>

# Try again
am start feat --agent claude
```

### Where can I report bugs or request features?

GitHub Issues: https://github.com/dstanek/am/issues

Include:
- Your OS and version
- Container runtime (Docker/Podman) version
- `am --version`
- Output of `am generate-config`
- Steps to reproduce

---

## Performance & Optimization

### Why is my container so slow?

**Possible causes:**
- Large image (slow to pull)
- Slow disk I/O in the container
- Slow network (if pulling image from registry)
- High CPU usage from agent process

**Fixes:**
- Use `-minimal` images
- Pre-pull images
- Monitor container with `docker stats` / `podman stats`

### How much disk space does `am` use?

Each session stores:
- Worktree (copy of your repo, usually a few MB–100s MB)
- Container image (300 MB–2 GB depending on image)
- Git objects (shared, not duplicated per session)

Multiple sessions reuse the same image, so disk overhead per session is mainly the worktree size.

### Can I store worktrees on a fast SSD?

Yes! By default worktrees go in `.am/worktrees/<slug>`. You can:
- Move `.am/` to a faster disk
- Use symlinks: `ln -s /mnt/fast/.am .am`
- Configure `am` to use a different path (not yet implemented, but requested)

---

## When Something Goes Wrong

**Session won't start?**
→ Check [Troubleshooting Guide](../guides/troubleshooting.md) or run `am generate-config`

**Container authentication fails?**
→ See [Claude Code Guide](../guides/claude-code.md#tips) or [Copilot Guide](../guides/github-copilot.md#tips)

**Tmux acting weird?**
→ Restart tmux: `tmux kill-session -t am-<slug>`

**Still stuck?**
→ Open an issue on GitHub with `am generate-config` output and error logs

# am — Agent Manager

`am` creates isolated environments for coding agents (Claude Code, GitHub Copilot, Gemini,
Codex, and others). Each session gets its own `am/<slug>` branch via a git worktree or jj
workspace, a dedicated split-pane tmux window, and — by default — a Podman or Docker container.
Run several agents in parallel on the same repository without them ever interfering with one
another, then tear each one down with a single command.

📖 **Full documentation:** <https://dstanek.github.io/agent-manager>

## Install

The install script downloads the right binary for your platform into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/dstanek/agent-manager/main/install.sh | sh
```

Or build from source (requires [Rust](https://rustup.rs) 1.70+):

```sh
git clone https://github.com/dstanek/agent-manager.git
cd agent-manager
cargo install --path .
```

Homebrew, `.deb`/`.rpm` packages, and Windows builds are also available — see the
[installation guide](https://dstanek.github.io/agent-manager/getting-started/installation/).

**Prerequisites:** a git or jj repository, Podman or Docker (unless you use `--no-container`),
a container image with your agent installed, and optionally tmux for split-pane sessions.

## Quick start

New to `am`? Run the guided setup — it walks you through the questions it can't answer on its own, then verifies the result:

```sh
am setup                             # guided: init + a couple of questions + a readiness check
am start feat --agent claude         # new branch, tmux window, container, launches Claude Code
```

Already know what you want? `am init` is the same setup without the questions:

```sh
am init                              # set up .am/ in the current repo
am start feat --agent claude         # new branch, tmux window, container, launches Claude Code
am attach feat                       # jump back to a running session
am list                              # see all active sessions
am destroy feat                      # stop container, kill window, remove branch
```

Run agents on several workstreams at once — each is fully isolated:

```sh
am start feature-api   --agent claude
am start feature-tests --agent copilot
am start feature-docs  --agent gemini
```

## Configuration

`am` merges configuration from CLI flags → `AM_*` env vars → `.am/config.toml` (project) →
`~/.config/am/config.toml` (global) → built-in defaults. Generate an annotated template with:

```sh
am generate-config > ~/.config/am/config.toml
```

A minimal global config — pick a default agent (its container image comes from the compiled-in
defaults, or override it under `[agents.<name>]`):

```toml
[defaults]
agent = "claude"

[agents.claude]
image = "ghcr.io/dstanek/am-claude-minimal:latest"
```

See the [configuration reference](https://dstanek.github.io/agent-manager/reference/configuration/)
for every option.

## Supported agents

Each agent has an auth preset that provides credentials to the container at runtime.

| Agent    | `--agent` value | Credentials provided                                            |
|----------|-----------------|-----------------------------------------------------------------|
| Claude Code   | `claude`   | mounts `~/.claude` and `~/.claude.json`                         |
| GitHub Copilot| `copilot`  | mounts `~/.config/gh` and `~/.config/github-copilot`           |
| Google Gemini | `gemini`   | mounts `~/.gemini`                                              |
| OpenAI Codex  | `codex`    | passes `OPENAI_API_KEY` through as an environment variable      |

Unknown agent names are rejected before any session resources are created.

## Example Dockerfile

Agents run inside a container, so the agent software must live in the image. This minimal
Alpine image installs Claude Code and works with `am` out of the box (credentials are mounted
at runtime, never baked in):

```dockerfile
FROM alpine:3.21

RUN apk add --no-cache bash ca-certificates curl git jujutsu

# Non-root user so Claude Code can run with --dangerously-skip-permissions
RUN addgroup -g 1000 am \
 && adduser -D -u 1000 -G am -s /bin/bash am \
 && mkdir -p /home/am/.config /workspace \
 && chown -R am:am /home/am/.config /workspace

USER am
ENV HOME=/home/am

# Native installer — no Node.js/npm required
RUN curl -fsSL https://claude.ai/install.sh | bash
ENV PATH="/home/am/.local/bin:${PATH}"
ENV DISABLE_AUTOUPDATER=1

# ~/.claude is mounted by `am` at runtime
WORKDIR /workspace
```

More examples for Python, Go, Rust, and Terraform live in [`examples/`](examples/); see the
[custom images guide](https://dstanek.github.io/agent-manager/guides/custom-images/) for details.

## License

MIT © David Stanek

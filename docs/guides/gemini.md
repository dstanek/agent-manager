# Gemini

`am` ships an auth preset for the Google Gemini CLI. Unlike Claude Code and Copilot, there is **no pre-built image** — you supply one. This guide covers what the preset does, the image you have to provide, and how to configure a project.

---

## Prerequisites

- **The Gemini CLI, authenticated on your host.** `am` mounts your existing credentials rather than performing a login of its own.
- **`~/.gemini/` must exist.** The preset mounts this directory, and `am start` refuses to run without it:

```
agent 'gemini' requires path to exist: /home/you/.gemini
Make sure gemini is installed and authenticated on this system
```

If the directory is missing, authenticate with the Gemini CLI on your host first. Once `defaults.agent = "gemini"` is configured, `am doctor` reports the same problem before you start a session.

---

## Container image

There is no compiled-in default image for `gemini` — `am` ships those only for `claude` and `copilot`. Starting a session without configuring one fails at preflight:

```
no container image configured — set an agent with `--agent` or `defaults.agent` in config
(image is selected automatically), or set `container.image` for a custom image
```

Point `am` at an image that contains the Gemini CLI:

```toml
# .am/config.toml
[agents.gemini]
image = "ghcr.io/your-org/am-gemini:latest"
```

The `dockerfiles/` directory in this repository is a reasonable starting point — `Dockerfile.claude-minimal` shows the shape of a minimal agent image (git plus the agent binary), with the agent swapped for the Gemini CLI.

!!! note "Credentials are never baked in"

    Authentication comes from the mounted `~/.gemini` at run time, so an image built this way holds no secrets and is safe to publish.

---

## Project configuration

```toml
# .am/config.toml
[defaults]
agent = "gemini"

[agents.gemini]
image = "ghcr.io/your-org/am-gemini:latest"
```

With `defaults.agent` set, `am start <slug>` uses Gemini without a flag. To use it for a single session instead, pass `--agent gemini`.

---

## Starting a session

```sh
am start review-api --agent gemini
```

Check the setup first with `am doctor`, which reports the image, whether `~/.gemini` is present, and everything else `am start` would check. It inspects the *configured* agent — it takes no `--agent` flag — so set `defaults.agent` first if you want it to check Gemini rather than whatever the project defaults to.

---

## What gets mounted

| Host path | Container path | Mode |
|---|---|---|
| `~/.gemini` | `<container home>/.gemini` | read-only |

The read-only mount means the container cannot modify your host credentials. If the Gemini CLI ever needs to write there — refreshing a token, for example — that write fails inside the session and the credential has to be refreshed on the host.

The usual session mounts apply as well: the worktree, the VCS directory, `~/.gitconfig`, and `~/.ssh`. See [Concepts](../concepts.md#container-isolation).

---

## Dev Container mode

In [Dev Container mode](devcontainers.md), the agent has to get into the image somehow. Claude Code has an official Dev Container Feature and is mapped out of the box; Gemini has none, so `agent_install = "auto"` falls through to `bootstrap`, which installs the CLI into a shared volume at run time.

If a Feature is published later, map it directly:

```toml
[agents.gemini]
devcontainer_feature = "ghcr.io/someone/gemini:1"
```

---

## Autonomous mode

`--auto` is accepted for Gemini but adds no agent flags — `am` has no equivalent of Claude Code's `--dangerously-skip-permissions` for this CLI. The session still runs in a container, and `--auto` still refuses to run with `--no-container`, but the agent itself will prompt as usual.

---

## Tips

- Credentials live only in the mount. Rotating them on the host takes effect in the next session, with no image rebuild.
- If several projects need different Gemini images, set `[agents.gemini].image` per project in `.am/config.toml` and commit it — that file is meant to be shared.
- `am doctor` reuses the same preflight functions as `am start`, so a passing report and a working start cannot drift apart.

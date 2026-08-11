# Codex

`am` ships an auth preset for the OpenAI Codex CLI. Codex is the only agent with **two independent ways to authenticate**, and `am` supports both. Like Gemini, it has no pre-built image — you supply one.

---

## Prerequisites

You need *one* of the following. Not both.

- **An interactive sign-in.** Run `codex` on your host and complete the sign-in; it persists credentials to `~/.codex/auth.json`. `am` mounts the directory into the session.
- **An API key.** Export `OPENAI_API_KEY` in the shell you run `am start` from, and `am` passes it into the container.

With neither, `am start` stops at preflight:

```
agent 'codex' has no credentials: OPENAI_API_KEY is not set and ~/.codex does not exist
Run 'codex' once to sign in, or export OPENAI_API_KEY=sk-...
```

If you have both, both are provided and Codex decides which to use.

---

## Container image

There is no compiled-in default image for `codex` — `am` ships those only for `claude` and `copilot`. Without one, preflight fails:

```
no container image configured — set an agent with `--agent` or `defaults.agent` in config
(image is selected automatically), or set `container.image` for a custom image
```

Point `am` at an image containing the Codex CLI:

```toml
# .am/config.toml
[agents.codex]
image = "ghcr.io/your-org/am-codex:latest"
```

`dockerfiles/Dockerfile.claude-minimal` in this repository shows the shape of a minimal agent image — git plus the agent binary — with the agent swapped for the Codex CLI.

---

## Project configuration

```toml
# .am/config.toml
[defaults]
agent = "codex"

[agents.codex]
image = "ghcr.io/your-org/am-codex:latest"
```

If you authenticate by API key and want it available without exporting it each time, add it to the container environment pass-through:

```toml
[container]
env = ["OPENAI_API_KEY"]
```

That passes the variable through from your host shell; it does not store the key in the file. Never put the key itself in `.am/config.toml` — that file is meant to be committed.

---

## Starting a session

```sh
am start refactor-api --agent codex
```

`am doctor` reports whether credentials were found, but it inspects the *configured* agent and takes no `--agent` flag, so set `defaults.agent` first if you want it to check Codex.

---

## What gets mounted

| Host path | Container path | Mode |
|---|---|---|
| `~/.codex` (when it exists) | `<container home>/.codex` | read-write |

Plus `OPENAI_API_KEY` as an environment variable when it is set in your shell.

Two details worth understanding:

**The whole directory is mounted, not just `auth.json`.** Codex replaces that file when it rotates a token, and a single-file bind mount would leave the container writing to an inode your host never sees — the refreshed token would vanish when the session ended.

**It is mounted read-write, not read-only.** A read-only mount works right up until the first token refresh, then fails.

!!! warning "State is shared with your host and between sessions"

    `~/.codex` holds more than credentials: `history.jsonl`, `sessions/`, and several SQLite databases with their write-ahead logs. Because the whole directory is mounted, your host Codex and every concurrent `am` session read and write the same files. Running several Codex sessions at once may interleave their history.

The usual session mounts apply as well: the worktree, the VCS directory, `~/.gitconfig`, and `~/.ssh`. See [Concepts](../concepts.md#container-isolation).

---

## Dev Container mode

In [Dev Container mode](devcontainers.md), Codex has no published Dev Container Feature, so `agent_install = "auto"` falls through to `bootstrap`, installing the CLI into a shared volume at run time. If a Feature appears later, map it:

```toml
[agents.codex]
devcontainer_feature = "ghcr.io/someone/codex:1"
```

---

## Autonomous mode

`--auto` is accepted for Codex but adds no agent flags — `am` has no equivalent of Claude Code's `--dangerously-skip-permissions` for this CLI. The session still runs in a container, and `--auto` still refuses to run alongside `--no-container`, but Codex itself prompts as usual.

---

## Tips

- If Codex works on your host but fails inside a session, check that `~/.codex/auth.json` exists — that is the file carrying an interactive sign-in.
- API-key users need no `~/.codex` at all; `am` mounts it only when present, so an absent directory is not an error.
- Rotating credentials on the host takes effect in the next session, with no image rebuild.

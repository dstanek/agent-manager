# Configuration

`am` supports layered configuration so you can set machine-wide defaults in a global file, override them per-project, and further override specific values with environment variables or CLI flags at runtime. All layers are optional — if nothing is set, compiled-in defaults apply.

---

## Precedence order

Later entries win (highest precedence last):

1. **Compiled-in defaults** — built into the `am` binary; always present as a fallback
2. **Global config** (`~/.config/am/config.toml`) — machine-wide defaults for all projects
3. **Project config** (`.am/config.toml`) — per-repository overrides
4. **Environment variables** — `AM_*` variables override both config files; useful in CI or for one-off changes without editing files
5. **CLI flags** (`--agent`, `--no-container` on `am start`) — highest precedence; affect only the single invocation

---

## Global config

**Path:** `~/.config/am/config.toml`

The global config sets machine-wide defaults that apply to every project. It is loaded on every `am` invocation before the project config is applied.

Generate a fully-documented template and place it in the standard location:

```sh
mkdir -p ~/.config/am
am generate-config > ~/.config/am/config.toml
```

Open the file and uncomment any values you want to change from the compiled-in defaults. Lines that remain commented out have no effect — the compiled-in default is used instead.

---

## Project config

**Path:** `.am/config.toml` (relative to the repository root)

The project config overrides global defaults for a specific repository. It is created automatically by `am init`. All lines are commented out by default so that global defaults flow through unchanged — uncomment only the keys you actually want to override.

This file is safe to commit, and committing it is the intended workflow: it is how a team shares one set of `agent`, `container`, and `devcontainer` defaults. `am init` ignores only `.am/worktrees/`, not `.am/` as a whole.

```sh
# Initialize a project (creates .am/config.toml)
am init
```

A minimal project config that sets the agent looks like this:

```toml
[defaults]
agent = "claude"
```

Selecting the agent also selects the container image — `am` ships with built-in image defaults for `claude` and `copilot`. You do not need to configure the image separately unless you are using a custom one.

---

## Unrecognised keys

A key `am` does not know is a warning, not an error. The file still loads and every key `am` does recognise still applies:

```
warning: unknown config key defaults.agnet in /home/you/src/project/.am/config.toml
```

`am doctor` reports the same keys as a check, grouped by the file each came from.

The keys are not rejected outright because `.am/config.toml` is meant to be committed and shared. A hard error would mean a config written against a newer `am` breaks for a teammate running an older one — so an unrecognised key is reported and ignored instead. The cost of that choice is that a typo does nothing silently until you read the warning, which is exactly what the warning is for.

---

## Environment variables

Environment variables override both the global and project configs and are useful for CI pipelines, Docker-in-Docker setups, or temporary one-off overrides without editing any files.

**Validation:** Enum fields (`AM_TMUX_SPLIT`, `AM_CONTAINER_MODE`, etc.) silently ignore unrecognised values so that adding new enum variants remains backwards-compatible. Numeric and identifier fields (`AM_TMUX_SPLIT_PERCENT`, `AM_CONTAINER_USER`) return a hard error if the value is out of range or malformed — the same behaviour as loading an invalid value from a config file.

### Config overrides

| Variable | Config key | Values | Example |
|---|---|---|---|
| `AM_AGENT` | `defaults.agent` | any non-empty string | `AM_AGENT=claude` |
| `AM_TMUX_AGENT_PANE` | `tmux.agent_pane` | `left`, `right` | `AM_TMUX_AGENT_PANE=right` |
| `AM_TMUX_SPLIT` | `tmux.split` | `horizontal`, `vertical` | `AM_TMUX_SPLIT=vertical` |
| `AM_TMUX_SPLIT_PERCENT` | `tmux.split_percent` | integer 1–99 (error if out of range) | `AM_TMUX_SPLIT_PERCENT=30` |
| `AM_CONTAINER_ENABLED` | `container.enabled` | `true`/`1`/`yes`, `false`/`0`/`no` | `AM_CONTAINER_ENABLED=false` |
| `AM_CONTAINER_MODE` | `container.mode` | `image`, `devcontainer`, `auto` | `AM_CONTAINER_MODE=devcontainer` |
| `AM_CONTAINER_RUNTIME` | `container.runtime` | `auto`, `podman`, `docker` | `AM_CONTAINER_RUNTIME=docker` |
| `AM_CONTAINER_IMAGE` | `container.image` | any non-empty string | `AM_CONTAINER_IMAGE=my-image:latest` |
| `AM_CONTAINER_NETWORK` | `container.network` | `full`, `none` | `AM_CONTAINER_NETWORK=none` |
| `AM_CONTAINER_USER` | `container.user` | safe username (`[a-z_][a-z0-9_-]*`, error if invalid) | `AM_CONTAINER_USER=am` |
| `AM_CONTAINER_GITCONFIG` | `container.gitconfig` | file path | `AM_CONTAINER_GITCONFIG=/custom/.gitconfig` |
| `AM_CONTAINER_SSH` | `container.ssh` | directory path | `AM_CONTAINER_SSH=/custom/.ssh` |
| `AM_DEVCONTAINER_PATH` | `devcontainer.path` | path relative to the worktree | `AM_DEVCONTAINER_PATH=.devcontainer/ci.json` |
| `AM_DEVCONTAINER_AGENT_INSTALL` | `devcontainer.agent_install` | `feature`, `bootstrap`, `none`, `auto` | `AM_DEVCONTAINER_AGENT_INSTALL=none` |
| `AM_DEVCONTAINER_ALLOW_HOST_COMMANDS` | `devcontainer.allow_host_commands` | `true`/`1`/`yes`, `false`/`0`/`no` | `AM_DEVCONTAINER_ALLOW_HOST_COMMANDS=true` |
| `CLAUDE_CONFIG_DIR` | (none) | directory path | `CLAUDE_CONFIG_DIR=/custom/.claude` |

!!! note "Mount path customization"
    `AM_CONTAINER_GITCONFIG` and `AM_CONTAINER_SSH` override where `am` looks for your git and SSH configuration on the host. `AM_CONTAINER_USER` changes the username used when constructing mount targets inside the container (e.g. `/home/<user>/.ssh`, `/home/<user>/.gitconfig`) and must be a safe POSIX username. `CLAUDE_CONFIG_DIR` overrides the Claude configuration directory. These are rarely needed unless you have a non-standard directory structure.

### Color

`am` colors status glyphs and the `error:`, `warning:` and `Note:` prefixes when it is writing to a terminal: green for fine, yellow for worth reading, red for something that will stop you. Color is only ever an accent on text that already says the same thing, so nothing is lost without it.

| Variable | Effect |
|---|---|
| `NO_COLOR` | Set to any non-empty value to disable color. Wins over everything else. |
| `CLICOLOR_FORCE` | Set to any non-empty value other than `0` to force color on, even when output is piped — useful for `less -R` or a CI log viewer that renders ANSI. |

Each stream is decided independently, so `am doctor > report.txt` still colors the warnings that go to stderr. Piped output is plain by default.

### Binary path overrides

These variables redirect `am` to a specific binary instead of searching `PATH`. Useful when a tool is installed in a non-standard location or when you want to pin to a specific version.

| Variable | Default binary | Description |
|---|---|---|
| `AM_TMUX_BIN` | `tmux` (from PATH) | Path or name of the tmux binary |
| `AM_PODMAN_BIN` | `podman` (from PATH) | Path or name of the Podman binary |
| `AM_DOCKER_BIN` | `docker` (from PATH) | Path or name of the Docker binary |
| `AM_JJ_BIN` | `jj` (from PATH) | Path or name of the Jujutsu binary |
| `AM_GH_BIN` | `gh` (from PATH) | Path or name of the GitHub CLI binary (used for Copilot auth) |
| `AM_DEVCONTAINER_BIN` | `devcontainer` (from PATH) | Path or name of the Dev Containers CLI (used only in devcontainer mode) |

If set to a bare name (e.g. `AM_TMUX_BIN=tmux3`), `am` searches PATH for that name. If set to an absolute path, it uses that path directly and errors if the file does not exist.

---

## CLI flags

The `am start` command accepts flags that act as the highest-precedence overrides for a single session:

| Flag | Description |
|---|---|
| `--agent <AGENT>` | Override the agent command for this session only. Must be a known agent integration: `claude`, `copilot`, `gemini`, `codex`. |
| `--no-container` | Disable container isolation for this session. The agent runs directly in the tmux pane. |

---

## Settings reference

### `[defaults]`

Top-level defaults that apply across all sessions unless overridden.

| Key | Type | Default | Description | Valid Values |
|---|---|---|---|---|
| `agent` | string | `""` | Default agent launched in the agent pane; also selects the container image via `[agents.<name>]`; empty means no agent is auto-launched | Any known agent name, e.g. `"claude"`, `"copilot"` |

!!! note "Version control is detected, not configured"

    There is no `vcs` setting. `am` looks for `.jj/` first and falls back to `.git/`, erroring if it finds neither, so a repository's own layout decides whether you get a jj workspace or a git worktree.

### `[agents.<name>]`

Per-agent configuration. `am` ships with compiled-in image defaults for `claude` and `copilot`; define an entry here to override them or to add images for other agents.

| Key | Type | Default | Description |
|---|---|---|---|
| `image` | string | see below | Container image to use when this agent is selected (used in `container.mode = "image"`) |
| `devcontainer_feature` | string | see below | Dev Container Feature that installs this agent, injected at build time in devcontainer mode |

**Compiled-in defaults:**

| Agent | Default image | Default Feature |
|---|---|---|
| `claude` | `ghcr.io/dstanek/am-claude-minimal:latest` | `ghcr.io/anthropics/devcontainer-features/claude-code:1` |
| `copilot` | `ghcr.io/dstanek/am-copilot-minimal:latest` | — |

Only Claude Code publishes an official Feature today. Agents without one fall through to the
`bootstrap` install path in devcontainer mode.

Example — override the claude image and add a gemini entry:

```toml
[agents.claude]
image = "my-org/am-claude:v2"

[agents.gemini]
image = "my-org/am-gemini:latest"
```

Agent entries are **merged** across config layers: global config entries extend the compiled-in defaults, and project config entries extend the global ones. Only keys you set in a later layer are overridden — other agents keep their values from earlier layers.

### `[tmux]`

Controls how the tmux window and panes are arranged for each session.

| Key | Type | Default | Description | Valid Values |
|---|---|---|---|---|
| `agent_pane` | string | `"left"` | Which pane receives the agent command after the split | `"left"`, `"right"` |
| `split` | string | `"horizontal"` | Direction of the tmux pane split | `"horizontal"`, `"vertical"` |
| `split_percent` | integer | `50` | Percentage of the total window given to the agent pane | 1–99 (error if out of range) |

### `[container]`

Controls container lifecycle and what gets mounted or exposed inside the container.

| Key | Type | Default | Description | Valid Values |
|---|---|---|---|---|
| `enabled` | boolean | `true` | Whether to run sessions inside a container | `true`, `false` |
| `mode` | string | `"auto"` | Where the environment comes from: the repo's own `.devcontainer/devcontainer.json` when one is found, an `am`-resolved image otherwise | `"auto"`, `"devcontainer"`, `"image"` |
| `runtime` | string | `"auto"` | Container runtime to use; `"auto"` tries Podman first, then Docker | `"auto"`, `"podman"`, `"docker"` |
| `image` | string | `""` | Override image for all agents; takes priority over `[agents.<name>].image`; leave unset to use the per-agent default | Any valid image reference |
| `network` | string | `"full"` | Network access mode for the container | `"full"` (unrestricted internet access), `"none"` (no network) |
| `env` | list of strings | `[]` | Extra environment variables passed into the container from the host shell | e.g. `["ANTHROPIC_API_KEY", "FOO=bar"]` |
| `gitconfig` | path | `""` | Host path to a gitconfig file to mount into the container; defaults to `$XDG_STATE_HOME/am/gitconfig`, which `am start` regenerates from your host `user.name` and `user.email`. Also the source for the `JJ_USER`/`JJ_EMAIL` variables described below | Any valid file path |
| `ssh` | path | `""` | Host path to an SSH directory to mount into the container; defaults to `~/.ssh` | Any valid directory path |
| `user` | string | `"am"` | Username used when building credential mount paths inside the container, such as `/home/<user>/.ssh` and `/home/<user>/.gitconfig`. In devcontainer mode the image's `remoteUser` takes precedence, and `root` resolves to `/root` rather than `/home/root` | safe username (`[a-z_][a-z0-9_-]*`) |

!!! note "jj identity"

    jj does not read git's identity, so a `jj` commit made inside a session container would otherwise be recorded with an empty committer — which jj refuses to push. When the mounted gitconfig supplies both a name and an email, `am` passes them into the container as `JJ_USER` and `JJ_EMAIL`. If either is missing from the gitconfig, neither variable is set, since a half-configured identity produces the same unpushable commit while looking correct. An explicit `JJ_USER`/`JJ_EMAIL` from `container.env`, a devcontainer, or your host environment takes precedence.

!!! note "Image selection"
    In most cases you do not need to set `container.image`. `am` resolves the image from the active agent via `[agents.<name>].image`, with built-in defaults for `claude` and `copilot`. Set `container.image` only when you want a single image to apply regardless of which agent is selected.

### `[devcontainer]`

Applies only when `container.mode` resolves to `devcontainer`. Building requires the
reference CLI (`npm install -g @devcontainers/cli`) and Node 20+; `am` builds an image once
per config change and runs it itself, so the CLI is not invoked on every session.

| Key | Type | Default | Description | Valid Values |
|---|---|---|---|---|
| `path` | path | `""` | Explicit `devcontainer.json`, relative to the session worktree; unset means discover | Any path inside the worktree |
| `cli` | string | `"devcontainer"` | CLI binary name or path (`AM_DEVCONTAINER_BIN` overrides) | Any binary name or path |
| `agent_install` | string | `"auto"` | How the agent gets into the image | `"feature"`, `"bootstrap"`, `"none"`, `"auto"` |
| `allow_host_commands` | boolean | `false` | Whether `initializeCommand`, `privileged`, `capAdd`, and `runArgs` are honoured | `true`, `false` |
| `skip_lifecycle` | boolean | `false` | Skip `postCreateCommand` and the other in-container hooks | `true`, `false` |
| `home` | path | `""` | Override the container home derived from `remoteUser` | Any absolute path |
| `extra_features` | table | `{}` | Extra Features to inject at build time, as `id = options-JSON` | e.g. `"ghcr.io/devcontainers/features/node:1" = "{}"` |

**Discovery order**, relative to the session *worktree* (not the repo root — the config is a
checked-in, branch-specific file):

1. `devcontainer.path`, if set
2. `.devcontainer/devcontainer.json`
3. `.devcontainer.json`
4. `.devcontainer/<folder>/devcontainer.json` — only when exactly one exists; several is an
   error listing the candidates

**Image caching.** The built image is named `am-dc-<hash>`, where the hash covers the config
bytes, the referenced Dockerfile, and any injected Features. Sessions sharing a config share
an image, and an unchanged config skips the build entirely.

!!! warning "Other build-context files are not hashed"
    Hashing an arbitrary build context is unbounded work — `"context": ".."` means the whole
    repo. Editing a file that your Dockerfile `COPY`s will not trigger a rebuild on its own;
    run `am start <slug> --rebuild` when that happens.

!!! danger "`initializeCommand` runs on your host"
    Of the six lifecycle hooks, `initializeCommand` is the only one that runs outside the
    container — on your machine, with your privileges — and `devcontainer.json` is
    repo-controlled code that arrives with a `git pull`. `am` refuses to run it unless
    `allow_host_commands = true`. The same flag gates `privileged`, `capAdd`, and `runArgs`;
    without it those are dropped with a note rather than failing the session.

**Lifecycle hooks.** `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, and
`postStartCommand` run inside the container before the agent starts. Because `am` runs
containers with `--rm`, every `am start` creates a fresh container and the create-time hooks
run each time — the previous container's filesystem is gone, so anything they installed must
be reinstalled. `postAttachCommand` is not run: `am attach` moves tmux focus, it does not
attach to the container.

**Not yet supported.** Configs using `dockerComposeFile` are rejected with a pointer to
`container.mode = "image"`. `userEnvProbe` and `forwardPorts` are parsed but not applied.

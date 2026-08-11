# Dev Containers

`am` can build a session's environment from your repo's own
`.devcontainer/devcontainer.json` instead of an `am`-specific image. If your project already
describes its toolchain for VS Code, GitHub Codespaces, or CI, that description works here
too — there is no second, `am`-shaped image to maintain.

## Requirements

Nothing beyond your usual Podman or Docker. `am` builds the image itself — resolving
Features from their registries, generating the Dockerfile, and handing the build to your
container runtime.

Some configs still need the reference CLI, and `am` falls back to it automatically when it
sees one:

- `dockerComposeFile`
- `overrideFeatureInstallOrder`, or a Feature that uses `dependsOn`
- a Feature referenced by local path (`./my-feature`) or by tarball URL

If you hit one, `am` tells you which construct caused it, and the CLI needs Node 20+:

```sh
npm install -g @devcontainers/cli
```

Either way a build happens once per config change, not once per session, so it stays off
the hot path.

If you would rather never depend on Node, make the fallback an error instead:

```toml
# .am/config.toml
[devcontainer]
builder = "native"   # "auto" (default) | "native" | "cli"
```

## Choosing a mode

`am` uses your devcontainer automatically when it finds one, so there is usually nothing to
turn on. To be explicit, or to opt out:

```toml
# .am/config.toml
[container]
mode = "devcontainer"   # or "image" to ignore .devcontainer/ entirely
```

Three modes are available:

| Mode | Behaviour |
|---|---|
| `auto` (default) | Use the repo's config when one is found, otherwise fall back to an image. |
| `devcontainer` | Use the repo's config; error if there isn't one. |
| `image` | Use an `am`-resolved image. Any `.devcontainer/` in the repo is ignored. |

`auto` is the default because a repo that has taken the trouble to describe its environment
almost certainly means for that description to be used — preferring an `am`-specific image
over it is the surprising behaviour. Repos with no `.devcontainer/` are unaffected.

If `am` cannot use your devcontainer, `mode = "image"` is the escape hatch, and the error
messages for unsupported constructs point at it.

## What happens on `am start`

```
am start my-feature
```

1. The worktree is created, and the config is discovered **inside it** — the config is a
   checked-in, branch-specific file, so a branch that changes it gets the changed version.
2. The config is hashed. The image is named `am-dc-<hash>`.
3. If that image does not exist, `devcontainer build` produces it. Build output streams to
   your terminal; a Feature install can take a few minutes.
4. `am` reads the built image's `devcontainer.metadata` label, merges it with the config,
   and runs the image with its own mounts, user mapping, and network policy.

A second session on an unchanged config reuses the image and never invokes the CLI at all.
After changing the config, the hash changes and the next `am start` rebuilds.

### Getting the agent into the image

Your project's devcontainer has *your* toolchain, not `claude`. `devcontainer.agent_install`
decides how the agent gets there:

| Value | Behaviour |
|---|---|
| `feature` | Inject the agent's Dev Container Feature at build time, baked into the cached image |
| `bootstrap` | Install into a shared volume at run time; works on any base image |
| `none` | The devcontainer already provides the agent |
| `auto` (default) | `feature` when one is mapped for the agent, otherwise `bootstrap` |

Claude Code has an official Feature
(`ghcr.io/anthropics/devcontainer-features/claude-code:1`) and is mapped out of the box. The
other agents have no published Feature and fall through.

To add one for an agent that gains a Feature later:

```toml
[agents.gemini]
devcontainer_feature = "ghcr.io/someone/gemini:1"
```

## Trust

A `devcontainer.json` is code, it lives in the repo, and it arrives with a `git pull` — while
`am` exists to *isolate* agents. Two things follow.

**`initializeCommand` runs on your host.** It is the one lifecycle hook that executes outside
the container, on your machine, with your privileges. `am` refuses to run it unless you say
otherwise:

```toml
[devcontainer]
allow_host_commands = true
```

The delegated build cannot run it either: `devcontainer build` neither executes
`initializeCommand` nor records it in the image, so the only host-side execution in
devcontainer mode is `am`'s own.

**Escalating options are dropped, not honoured.** `privileged`, `capAdd`, and `runArgs` are
ignored by default, with a note on stderr saying what was skipped. The same
`allow_host_commands` flag grants them. Dropping rather than failing is deliberate — most
containers work fine without `privileged`, and refusing to start over a capability the
session may not need would be worse than starting without it.

Note also what devcontainer mode means for credentials: `~/.ssh` and your agent's config get
mounted into an image the *repo* defines. That is the same exposure as a custom
`container.image`, but it becomes the common path rather than an escape hatch.

`container.network = "none"` still applies to the run step. The build step needs network
access to fetch Features, so it is unaffected.

## Lifecycle hooks

`onCreateCommand`, `updateContentCommand`, `postCreateCommand`, and `postStartCommand` run
inside the container, in that order, before the agent starts. Disable them with
`skip_lifecycle = true`.

Because `am` runs containers with `--rm`, every `am start` creates a fresh container, so the
create-time hooks run every time rather than once. This is not a shortcut: the previous
container's filesystem is gone, so anything `postCreateCommand` installed has to be installed
again.

`postAttachCommand` is not run. `am attach` moves tmux focus; it does not attach to the
container, so there is no attach event to hang it off.

## Worktrees and workspaces

Both git worktrees and jj workspaces work. `am` mounts the worktree and the VCS directory at
their *same absolute host paths* inside the container, so the relative path in a jj
workspace's `.jj/repo` and the absolute `gitdir:` in a git worktree's `.git` file both
resolve correctly. Nothing about your VCS choice needs special handling.

## Rebuilding

```sh
am start my-feature --rebuild
```

The config hash covers the `devcontainer.json` bytes, the referenced Dockerfile, and any
injected Features — but not other files in the build context. Hashing an arbitrary context is
unbounded work (`"context": ".."` means the whole repo), so if you edit a file your Dockerfile
`COPY`s, use `--rebuild`.

## Not supported yet

| Construct | Status |
|---|---|
| `dockerComposeFile` | Rejected with a pointer to `container.mode = "image"` |
| `userEnvProbe` | Parsed, not applied — interactive panes get the login environment anyway |
| `forwardPorts` | Parsed, not applied |
| `postAttachCommand` | Not run (see above) |

## Troubleshooting

Start with `am doctor`. It reports the discovered config, the CLI and its version, Node,
whether the built image is current, and any construct `am` will refuse or drop — which
covers most of what follows before you have to guess.

**"devcontainer CLI not found"** — your config uses something `am` cannot build itself, so
it tried to fall back. Install `@devcontainers/cli` globally, point `devcontainer.cli` (or
`AM_DEVCONTAINER_BIN`) at it, or switch to `container.mode = "image"`. `am doctor` reports
this as "not needed" when the config is one `am` can build on its own.

**"am's own builder cannot handle this config"** — you set `builder = "native"`, which turns
the CLI fallback into an error. The message names the construct; set `builder = "auto"` to
fall back instead.

**The build succeeded but the agent isn't found** — the image has no agent and
`agent_install` resolved to `none`. Check that your agent has a mapped Feature, or set
`agent_install = "bootstrap"`.

**Several configs found** — `am` will not guess between multiple
`.devcontainer/<folder>/devcontainer.json` files. Set `devcontainer.path` to pick one.

**A failed `am start` used to leave a worktree behind** — it no longer does. If a build,
trust check, or tmux call fails, the worktree and its branch are rolled back, and the same
slug is immediately reusable.

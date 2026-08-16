# Dev Containers

`am` can build a session's environment from your repo's own
`.devcontainer/devcontainer.json` instead of an `am`-specific image. If your project already
describes its toolchain for VS Code, GitHub Codespaces, or CI, that description works here
too — there is no second, `am`-shaped image to maintain.

## Requirements

Nothing beyond your usual Podman or Docker. `am` builds the image itself — resolving
Features from their registries, generating the Dockerfile, and handing the build to your
container runtime.

Features work whether they come from a registry, from a directory in your repo
(`./my-feature`), or from a tarball URL — and whether your config names them directly or
another Feature pulls them in through its `dependsOn`.

Compose projects work too. `am` builds the service your `devcontainer.json` names, brings the
whole project up, and runs the agent inside that service — so the database or cache the project
depends on is there, the way it would be in any other editor. `am destroy` takes the project
down again.

Three things to know about compose sessions:

- The config **must** name a `service`. Without it there is nothing to say which container the
  agent belongs in.
- Keeping that service alive is the compose file's job, not `am`'s. The devcontainer convention
  is `command: sleep infinity`; a service that exits immediately leaves the session with nowhere
  to run.
- `container.network = "none"` cannot apply, because compose services reach each other over the
  project network. `am` refuses rather than quietly ignoring it.

**There is no Node dependency, and no way to reintroduce one.** `am` builds every config shape
the spec defines and never shells out to `@devcontainers/cli`. A `devcontainer.json` `am` cannot
build is one the reference CLI rejects too, and `am` says so directly.

Builds happen once per config change, not once per session.

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

## The agent's environment

A devcontainer's toolchain is often installed by something that appends to `PATH` in a dotfile —
nvm, rbenv, sdkman, a Feature's own line in `.bashrc`. A process started directly in the
container never sources those, so the agent would not see tools that are plainly there in an
editor terminal.

`userEnvProbe` is the spec's answer, and `am` applies it: before the agent starts, `am` runs your
login shell in the container, reads the environment it ends up with, and applies it. The default
is `loginInteractiveShell`, so a config that says nothing still gets this.

```jsonc
{
  // "loginInteractiveShell" (default) | "loginShell" | "interactiveShell" | "none"
  "userEnvProbe": "none"
}
```

Two things `am` guarantees here. The probe runs in a *throwaway* process, so a `.bashrc` that
prints a banner does not end up in the agent's own process tree. And variables `am` set
deliberately — `containerEnv`, `remoteEnv`, your agent's credentials, the jj identity — are never
overwritten by what the probe finds, so a dotfile cannot quietly undo the session's setup.

Image-mode sessions are unaffected: there is no devcontainer config to ask for a probe, and their
behaviour is unchanged.

## Ports

`forwardPorts` publishes each port on `127.0.0.1`, so a server started inside the session is
reachable at the same port on your machine.

This is a deliberate difference from the reference CLI, which publishes nothing for
`forwardPorts` and leaves the forwarding to an editor. `am` has no editor, so the alternative
would be for the key to do nothing at all. Loopback rather than every interface, because a
session container is not something to put on the network by default.

In a compose project, a bare port is published on the agent's service and a
`"<service>:<port>"` entry on the service it names — the one case where `am` writes an override
for a service it does not run the agent in. Outside compose, `"<service>:<port>"` has nothing to
refer to and is skipped.

A port that is already taken on your machine will fail the session start, the same as any other
publish conflict.

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

`postAttachCommand` runs on every attach, which is what the spec asks for. Starting a session
counts as attaching to it, so it runs there too, chained after the hooks above. When `am attach`
finds a session that is already live — the case where it only moves tmux focus — the hook is
`exec`'d into the running container instead, since there is no new container command to chain it
into. A config with no `postAttachCommand` execs nothing.

## Worktrees and workspaces

Both git worktrees and jj workspaces work. `am` mounts the worktree and the VCS directory at
their *same absolute host paths* inside the container, so the relative path in a jj
workspace's `.jj/repo` and the absolute `gitdir:` in a git worktree's `.git` file both
resolve correctly. Nothing about your VCS choice needs special handling.

## The lockfile

`am` reads and writes `.devcontainer/devcontainer-lock.json`, the same file the Dev Containers
tooling uses. It records the exact artifact each Feature resolved to:

```json
{
  "features": {
    "ghcr.io/devcontainers/features/git:1": {
      "version": "1.3.8",
      "resolved": "ghcr.io/devcontainers/features/git@sha256:fd75…",
      "integrity": "sha256:fd75…"
    }
  }
}
```

It does two things. **Pinning**: `…/git:1` is a moving tag, so without a lockfile two people
building the same config can get different Features; with one, `am` fetches the recorded digest.
**Rebuild detection**: a Feature from a registry cannot be hashed without asking the registry,
and doing that on every `am start` would defeat the point of caching images — so `am` hashes the
lockfile instead. Move the pin and the image name changes and the next `am start` rebuilds.

Commit it. It belongs to the repo the way any lockfile does, and `am` writes it into the session
worktree during a build, so it will show up as a change there.

Two things to know:

- **Adding a lockfile to a repo that had none renames the image once**, so the next `am start`
  rebuilds. It is mostly a layer-cache hit, and the alternative is never noticing a moved tag.
- **A tarball Feature whose bytes changed is an error, not a silent update.** `integrity` is the
  only way to detect that, and quietly installing different code than the lockfile records would
  make the file worse than useless. Delete the entry to accept the new contents.

Local Features are not recorded — the spec excludes them, and `am` hashes their files directly,
which is both cheaper and exact.

## Rebuilding

```sh
am start my-feature --rebuild
```

The config hash covers the `devcontainer.json` bytes, the referenced Dockerfile, any injected
Features, the contents of any Feature vendored in the repo, and the lockfile — so editing a
vendored Feature or moving a pin rebuilds on its own.

It does not cover other files in the build context. Hashing an arbitrary context is unbounded
work (`"context": ".."` means the whole repo), so if you edit a file your Dockerfile `COPY`s,
use `--rebuild`.

## Not supported yet

| Construct | Status |
|---|---|
| `portsAttributes` | Carried in the label, not acted on — it describes ports to an editor |

## Troubleshooting

Start with `am doctor`. It reports the discovered config, whether the built image is current,
and any construct `am` will refuse or drop — which covers most of what follows before you have
to guess.

**"this devcontainer.json has nothing to build from"** — the config names no `image`, no
`build.dockerfile` and no `dockerComposeFile`, so there is nothing to build. This is not an `am`
limitation: the reference CLI rejects the same config with "No image information specified in
devcontainer.json", and `build.dockerfile` has no default there either.

**The build succeeded but the agent isn't found** — the image has no agent and
`agent_install` resolved to `none`. Check that your agent has a mapped Feature, or set
`agent_install = "bootstrap"`.

**Several configs found** — `am` will not guess between multiple
`.devcontainer/<folder>/devcontainer.json` files. Set `devcontainer.path` to pick one.

**A failed `am start` used to leave a worktree behind** — it no longer does. If a build,
trust check, or tmux call fails, the worktree and its branch are rolled back, and the same
slug is immediately reusable.

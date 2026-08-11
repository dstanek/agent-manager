# Commands

Complete reference for all `am` commands.

---

## `am init`

Initialize a new `am` project in the current repository.

**Usage**

```sh
am init
```

**What it does**

- Creates the `.am/` directory at the repository root
- Writes a default `.am/config.toml` with all settings commented out
- Appends `.am/worktrees/` to `.gitignore` (creates `.gitignore` if it does not exist)
- Prints an advisory if `.gitignore` still contains a broad `.am/` entry, since `.am/config.toml` is meant to be committed

`am init` does not create a session state file. Sessions are recorded in a per-user store at `$XDG_STATE_HOME/am/sessions.json` (falling back to `~/.local/state/am/sessions.json`), created on demand by `am start`.

Running `am init` in a directory that is not a git or jj repository is an error. Running it a second time in the same repository is safe — existing files are not overwritten.

!!! note
    `am init` must be run from inside a git or jj repository. `am` detects `.jj/` first; if not found it checks for `.git/`. If neither is present, the command exits with an error.

---

## `am doctor`

Report what is and is not ready for a successful `am start`, without changing anything.

**Usage**

```sh
am doctor
```

**What it checks**

| Section | Checks |
|---|---|
| Repository | Inside a git or jj repo |
| Project setup | `.am/` initialized, git identity available |
| tmux | Binary present, and whether you are currently inside a session |
| Container runtime | Podman or Docker present (respects `container.runtime`); whether an SSH agent will be forwarded |
| Environment | Where the environment comes from, and whether that source is usable |
| Agent | Selected agent is known, and its credentials are present on this host |

In dev container mode the Environment section additionally reports the discovered
config, the `devcontainer` CLI and its version, Node 20+, whether the built image is
current for the config hash, and any construct `am` will refuse (`dockerComposeFile`) or
drop (`initializeCommand`, `runArgs`).

**Output**

Each check is `✓` (ready), `!` (usable, worth knowing), or `✗` (will stop `am start`),
with the fix indented underneath:

```
Environment
  ✓ source                 devcontainer at /path/to/.devcontainer/devcontainer.json
  ✗ devcontainer CLI       'devcontainer' not found on PATH
      → npm install -g @devcontainers/cli (needs Node 20+), or set container.mode = "image"
  ✓ node                   v22.23.2 (>= 20 required)
  ! built image            am-dc-f260010a69f5 not built yet
      → the next 'am start' will build it — this can take a few minutes
```

**Exit code**

`0` when nothing would stop `am start`, `1` otherwise. Warnings alone do not fail, so
`am doctor && am start feat` is a usable setup gate.

!!! note "Discovery uses your current checkout"
    A session gets a fresh worktree off `HEAD`, so an *uncommitted* `devcontainer.json`
    is not what that session will see. `am doctor` reports what is on disk now.

---

## `am start <slug>`

Create a new isolated agent session.

**Usage**

```sh
am start <slug> [OPTIONS]
```

**Arguments**

| Argument | Description |
|---|---|
| `<slug>` | Session name. Must be 1–40 characters, using only lowercase letters (`a–z`), digits (`0–9`), hyphens (`-`), and underscores (`_`). |

**Options**

| Option | Description |
|---|---|
| `--agent <AGENT>` | Agent command to launch in the session's agent pane. Overrides the `agent` value from config. Must be one of: `claude`, `copilot`, `gemini`, `codex`. Unknown values are rejected with an error. See [Concepts](../concepts.md#agent-integrations) for details. |
| `--no-container` | Disable container isolation for this session. The agent command will run directly in the tmux pane instead of inside a container. |
| `--auto` | Launch the agent in autonomous mode (passes agent-specific flags to skip interactive prompts, e.g. `--dangerously-skip-permissions` for Claude). Requires `--agent` and container to be enabled. |
| `--rebuild` | Rebuild the dev container image even if one already exists for this config. Only meaningful in devcontainer mode; use it after changing a file the config hash does not cover, such as something your Dockerfile `COPY`s. |

**What it does**

1. Validates the slug, detects the container runtime, and checks the selected agent's credentials — everything that can fail before anything is created
2. Creates a git worktree at `.am/worktrees/<slug>` on a new `am/<slug>` branch (or a jj workspace if the repo uses jj)
3. In devcontainer mode: discovers `.devcontainer/devcontainer.json` **inside the worktree**, builds it into `am-dc-<hash>` if that image does not already exist, and reads the built image's metadata
4. If inside tmux: opens a new window named `am-<slug>` with a split pane; sets up the agent pane and the shell pane
5. If container is enabled: launches the container with the appropriate mounts and environment variables
6. Sends the agent command to the agent pane
7. Records the session in the global session store, tagged with the repository it belongs to

Steps 3–6 are covered by a rollback: if any of them fails, the worktree and its branch are
removed rather than left behind for you to clean up by hand.

If `am start` is run outside of tmux, it creates the worktree and then launches the container directly (replacing the current shell process via `exec()`). No tmux window is created.

---

## `am list`

List active sessions — by default those belonging to the current project.

**Usage**

```sh
am list
am list --all
```

**Options**

| Flag | Description |
|---|---|
| `--all` | Show sessions from every repository, not just the current one. |

Reads the global session store and prints a table of matching sessions. If there are none, prints a friendly message instead.

Plain `am list` must be run inside a git or jj repository, since it filters by the current repo; if you are outside one, `am` says so and points you at `am list --all`. `am list --all` works from any directory.

The first time `am list` runs in a repository that still has an old `.am/sessions.json`, its records are migrated into the global store and the old file is removed. This happens transparently — no action is needed on your part.

**Output columns**

| Column | Description |
|---|---|
| `REPO` | Repository the session belongs to, with `$HOME` abbreviated to `~`. Only shown with `--all`. |
| `SLUG` | The session name |
| `CONTAINER` | Container runtime in use (`podman`, `docker`), or `—` if no container |
| `AUTO` | `yes` if the session was started with `--auto`, otherwise `—` |
| `WORKTREE` | Absolute path to the session's git worktree or jj workspace |
| `WINDOW` | The tmux window name (`am-<slug>`) |
| `STATUS` | `stale` if the session's repository no longer exists on disk, otherwise blank. Only shown with `--all`. |
| `CREATED` | Timestamp when the session was created (`YYYY-MM-DD HH:MM`) |

With `--all`, sessions are grouped by repository and sorted oldest-first within each, and any `stale` rows are sorted to the bottom. A stale row means the repository was moved or deleted without `am destroy` being run first. Clear it with [`am session rm <slug> --repo <path>`](#am-session-rm-slug) — `am destroy` is no help here, because it needs the repository it is being asked to clean up.

**Example output**

```
SLUG    CONTAINER  AUTO  WORKTREE                          WINDOW     CREATED
feat    podman     —     /home/user/proj/.am/worktrees/feat am-feat    2026-04-12 09:00
bugfix  —          —     /home/user/proj/.am/worktrees/bugfix am-bugfix 2026-04-12 08:47
```

---

## `am attach <slug>`

Attach to an existing session's tmux window.

**Usage**

```sh
am attach <slug>
```

Switches the current tmux client to the `am-<slug>` window. If the window does not exist (for example, after a system restart), `am attach` creates a new window and split for the session — it does not error.

!!! warning "Requires tmux"
    `am attach` must be run from inside a tmux session. If `$TMUX` is not set, the command exits with an error. To get a terminal inside an existing session without tmux, navigate directly to `.am/worktrees/<slug>`.

---

## `am run <slug> <agent>`

Send an agent command to a session's agent pane.

**Usage**

```sh
am run <slug> <agent>
```

Uses `tmux send-keys` to send the specified agent command to the agent pane of the `am-<slug>` window, followed by Enter. This is useful for (re)starting an agent in a session that was started without one, or after the agent process has exited.

**Example**

```sh
am run feat claude
```

!!! warning "Requires tmux"
    `am run` must be run from inside a tmux session. If `$TMUX` is not set, the command exits with an error.

---

## `am destroy <slug>`

Destroy an agent session.

**Usage**

```sh
am destroy <slug> [OPTIONS]
```

**Options**

| Option | Description |
|---|---|
| `--force`, `-f` | Skip the confirmation prompt and proceed immediately. |

**What it does**

1. Stops the container (`podman stop am-<slug>` or equivalent)
2. Removes the container (`podman rm am-<slug>` or equivalent)
3. Kills the tmux window `am-<slug>` (skipped if the window no longer exists)
4. Removes the git worktree at `.am/worktrees/<slug>` and deletes the `am/<slug>` branch
5. Removes the session record from the global session store

Without `--force`, `am` prints a summary of what will be destroyed and asks for confirmation. This is the only destructive command in `am` and cannot be undone — the worktree and branch are permanently deleted.

---

## `am session rm <slug>`

Remove a session record from the global store. Use this when `am destroy` cannot do the job — most often for a `stale` row in `am list --all`, where the repository has been moved or deleted and there is no longer a worktree to tear down.

**Usage**

```sh
am session rm <slug>
am session rm <slug> --repo ~/src/old-project
am session rm <slug> --force
```

**Options**

| Flag | Description |
|---|---|
| `--repo <path>` | Repository the session belongs to. Required when you are not inside a repository, or when the same slug exists in more than one. The path need not still exist — that is the point for stale records. |
| `--force`, `-f` | Skip the confirmation prompt. |

**What it does**

1. Stops and removes the container, if the record names one — best effort, a failure is a warning rather than an error
2. Kills the tmux window — also best effort
3. Removes the session record from the global store

**What it does not do**

It leaves the worktree and branch alone. That is the difference from `am destroy`, which removes them and is the right command whenever the repository is still present. `am session rm` cleans up the bookkeeping, not your code.

**Resolving the slug**

Run inside a repository, `am` looks for the slug there first. If the slug exists in exactly one repository anywhere in the store, that one is used. If it exists in several, `am` lists the candidates and asks you to disambiguate with `--repo`:

```
slug 'feat' exists in multiple repos:
  /home/you/src/project-a
  /home/you/src/project-b
Use --repo <path> to specify which one.
```

Run outside any repository, the same rules apply minus the current-repo preference. An unknown slug is an error naming the slug.

---

## `am generate-config`

Print a fully-documented configuration template to stdout.

**Usage**

```sh
am generate-config
am generate-config > ~/.config/am/config.toml
```

Prints a complete `config.toml` template with every supported setting, its default value, and an explanatory comment. All settings are commented out so that the compiled-in defaults apply unless explicitly uncommented.

Useful for seeding either the global config or a project config:

```sh
# Create global config
mkdir -p ~/.config/am
am generate-config > ~/.config/am/config.toml

# Create project config (am init does this automatically)
am generate-config > .am/config.toml
```

---

## Slug validation

Slugs are the short names used to identify sessions. The following rules apply:

- **Length:** 1–40 characters
- **Characters:** only lowercase letters (`a–z`), digits (`0–9`), hyphens (`-`), and underscores (`_`)
- **Pattern:** `[a-z0-9_-]{1,40}`

Slugs that do not match these rules are rejected immediately by `am start` with an error message, before any side effects occur.

**Valid examples**

```
feat
fix-auth
my_feature
v2
release-2026-03
```

**Invalid examples**

```
MyFeature       # uppercase letters not allowed
fix auth        # spaces not allowed
-leading-dash   # must start with a letter or digit
```

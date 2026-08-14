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

**Example output**

A headline states the outcome, followed by the detail behind it. On a fresh repo:

```
Initialized am in this repo.
  Created .am/config.toml
  Added .am/worktrees/ to .gitignore

Run 'am start <slug>' to create your first session.
```

Re-running it once everything already exists drops the detail — it would only repeat what
the headline already says — and folds the next step onto its own indented line:

```
am is already initialized in this repo.
  Run 'am start <slug>' to create your first session.
```

A mixed run (say, `.am/config.toml` already existed but the `.gitignore` entry didn't) keeps
every detail line, since some of it is genuinely new and some isn't:

```
Initialized am in this repo.
  .am/config.toml already exists, skipping
  Added .am/worktrees/ to .gitignore

Run 'am start <slug>' to create your first session.
```

The `.gitignore`-advisory case groups the `Note:` line after the detail rather than
interleaving it where it was discovered, so it reads as one call-out attached to the list
rather than an interruption partway through it:

```
Initialized am in this repo.
  Created .am/config.toml
  Added .am/worktrees/ to .gitignore
  Note: .am/ is in .gitignore; .am/config.toml is now committable — you may want to narrow this to .am/worktrees/

Run 'am start <slug>' to create your first session.
```

Exit code and file behavior are unaffected either way — only the rendering changed.

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
with the fix indented underneath. Hints are concrete — an install link, an exact command to
run, a doc section to read — rather than naming the problem abstractly:

```
Environment
  ✓ source                 devcontainer at /path/to/.devcontainer/devcontainer.json
  ✗ devcontainer CLI       'devcontainer' not found on PATH
      → npm install -g @devcontainers/cli (needs Node 20+), or set container.mode = "image"
  ✓ node                   v22.23.2 (>= 20 required)
  ! built image            am-dc-f260010a69f5 not built yet
      → the next 'am start' will build it — this can take a few minutes
```

A missing container runtime and missing agent credentials get the same treatment:

```
Container runtime
  ✗ runtime                neither podman nor docker found on PATH
      → install Podman (https://podman.io/docs/installation) or Docker
        (https://docs.docker.com/get-docker/), or set container.enabled = false in
        .am/config.toml

Agent
  ✓ agent                  claude
  ✗ credentials            ~/.claude does not exist
      → run 'claude auth login' (or set ANTHROPIC_API_KEY) — see
        docs/guides/claude-code.md#prerequisites
```

Every agent's credentials hint names that agent's actual sign-in command (`gh auth login` for
`copilot`, a `codex` sign-in or `OPENAI_API_KEY` for `codex`, and so on) and links to that
agent's guide. These are the same hints [`am setup`](#am-setup) re-lists in its own "What to do
next" block on failure — strengthening one strengthens both, since `am setup`'s verification
step *is* this command.

**Exit code**

`0` when nothing would stop `am start`, `1` otherwise. Warnings alone do not fail, so
`am doctor && am start feat` is a usable setup gate.

!!! note "Discovery uses your current checkout"
    A session gets a fresh worktree off `HEAD`, so an *uncommitted* `devcontainer.json`
    is not what that session will see. `am doctor` reports what is on disk now.

---

## `am setup`

Guided, interactive setup — the on-ramp for a first-time user or a new repository. Runs `am init`'s setup, asks only the questions detected state can't answer, then verifies the result by running the same checks `am doctor` runs.

**Usage**

```sh
am setup
am setup --yes
am setup --agent claude
am setup --yes --agent claude
```

**Options**

| Option | Description |
|---|---|
| `--yes`, `-y` | Skip every prompt; each question resolves to its effective current value (project config → global config → compiled default). Never offers to start a session. Exits with the same code `am doctor` would (`0` clean, `1` on failure), so `am setup --yes && am start feat --agent claude` works as a CI bootstrap step. |
| `--agent <AGENT>`, `-a` | Set the agent directly instead of asking — evaluated the same way with or without `--yes`. If it differs from the current value, it is written; if it matches, nothing is written. Must be one of `claude`, `copilot`, `gemini`, `codex`; an unknown name is rejected immediately, before any file is touched. |

**What it does**

1. Requires an interactive terminal unless `--yes` is passed. With no TTY and no `--yes`, it fails immediately with "am setup requires an interactive terminal" — no file is touched.
2. Runs the same directory and `.gitignore` setup as `am init` (creates `.am/config.toml` if missing, adds `.am/worktrees/` to `.gitignore`), and creates `~/.config/am/config.toml` if it doesn't exist yet — both fully commented skeletons, same as `am init`'s output.
3. Asks which agent to use, unless `--agent` was given. Every question below opens with its own header line naming what's being asked, then a dimmed, indented line naming where the answer is saved — scope first, then the file path, e.g. `  just this repo; saved to .am/config.toml.` The prompt's default is the effective current value — project config, then global config, then (if nothing is configured anywhere) the first agent with credentials already detected on this host, falling back to `claude` — labeled with its source on its own dimmed line below the menu, e.g. `  currently: claude (from your global config)`. Note this can name a *different* file than the write-target line above the menu: the default may come from the global config, but a change is always written to the project file. Pressing Enter accepts the default; if that default is already what's configured, nothing is written. An agent whose credentials are present on this host is marked `credentials found` in its own aligned column, e.g. `[1] claude    credentials found` — presence-only, the same guarantee `am doctor`'s `credentials` check makes, so the wording never claims more than that. When nothing is configured anywhere and no agent has credentials found for it either, one extra line states the `claude` fallback explicitly: `nothing found configured or credentialed on this host — defaulting to claude.`
4. Asks about containers, in exactly one of two framings, chosen by whether `~/.config/am/config.toml` already existed *before this run*:
   - **Fresh setup (no global config yet):** always asked, regardless of whether a runtime is currently found — "Use isolated containers for your sessions?", explained, recommended, defaulted to yes. If no runtime was detected yet, one extra dim line says so without blocking the choice. This is the informed-consent framing: a newcomer may not know sessions run in containers at all, so the question isn't gated behind a detection failure the way the returning-setup framing is.
   - **Returning setup (a global config already exists):** the original framing, unchanged — asked only when neither Podman nor Docker is on `PATH` *and* there is a global config file to write the answer to: "Proceed with containers disabled for now?"

   These two are mutually exclusive per run — you are never shown both in the same invocation. Which runtime to use is never asked either way — `container.runtime = "auto"` already resolves that on its own.
5. Prints a one-line note when `.devcontainer/devcontainer.json` is found (sessions will use it automatically), and a warning if it declares `initializeCommand` — never a prompt, since defaulting host command execution on would be an unsafe wizard default.
6. Runs `am doctor`'s checks against whatever was just written (steps 3–4) and prints the identical report.
   - **Clean (0 failures):** continues to step 7.
   - **Failures:** prints a "What to do next:" block right after the report — every failing check's hint, one per line, in report order — then exits with the same code `am doctor` would (`1`). Steps 7–8 are not reached; you're sent back to fix the readiness problem before being asked anything cosmetic.
7. Asks for a pane layout — only reached once step 6 reports zero failures — unless `--yes` was passed or there is no global config file to write to. This question is deliberately asked *after* verification rather than alongside agent/containers: a "wrong" layout doesn't stop `am start` from working, so personalisation is deferred until the tool is confirmed to actually run a session. Unlike the agent and containers questions it is always asked once reached — pane layout is a genuine preference no amount of host detection can answer. A single menu offers four presets plus a customize option, each shown with a small ASCII preview:

   ```
   Which layout do you want?
     every repo on this machine; saved to ~/.config/am/config.toml.

     [1] agent left, 50/50
         [  agent   |  shell   ]
     [2] agent right, 50/50
         [  shell   |  agent   ]
     [3] agent left, 70/30
         [    agent    | shell ]
     [4] stacked, agent on top, 50/50
         [       agent        ]
         [       shell        ]
     [5] customize…

     currently: agent_pane=left, split=horizontal, split_percent=50 (am's default)

   Layout [1-5] (Enter to keep current): 
   ```

   `[5] customize…` asks direction (side by side or stacked) first, then a pane-side question worded to match — "left"/"right" for a side-by-side split, "top"/"bottom" for a stacked one — then a percentage (1-99), then previews the result with a proportion line (e.g. `agent (top) gets 95%, the other pane gets 5%.`) and a final "Use this layout? [Y/n]" confirmation. Declining the preview re-shows the preset menu rather than restarting just the last sub-question. Accepting a preset or a customized combination writes only the `tmux.*` keys that actually changed — picking a layout that only differs by percentage does not also rewrite `agent_pane`/`split`. If the project config already sets its own `[tmux]` values, one line is printed before the menu: "Note: this project's config already sets its own pane layout — your answer here is saved globally and won't change sessions in this repo until that override is removed."
8. If step 6 was clean and the session is interactive (not `--yes`, stdin is a TTY), offers to start a first session — accepting prompts for a slug and calls the same code path as `am start`. Declining, or running under `--yes`/non-interactively, prints a "Next steps" block instead and exits `0`.

**What it writes**

| Question | Written to | Key |
|---|---|---|
| Agent | `.am/config.toml` (project) | `defaults.agent` |
| Containers | `~/.config/am/config.toml` (global) | `container.enabled` |
| Pane layout | `~/.config/am/config.toml` (global) | `tmux.agent_pane`, `tmux.split`, `tmux.split_percent` |

Changing the agent always writes to the project file, even when the current value was inherited from the global config — the agent is a per-repo decision. `container.enabled` and the three `tmux.*` layout keys are host decisions and always go to the global file, one key at a time — each is only written if its own value actually changed. Both container framings write through the same key; accepting the fresh-setup default (yes) writes nothing, since containers enabled is already the compiled default — only a decline writes `container.enabled = false`.

Under `--yes`, the containers and pane-layout questions are skipped entirely and write nothing, even on a fresh repo — neither has an "unanswered means broken" stake the way an unset agent does. The agent question still resolves to a proactive best-guess default under `--yes` when nothing is configured.

Agent and container writes are **not** rolled back if the doctor check in step 6 still fails for an unrelated reason (e.g. you fixed your agent choice but the report still flags missing git identity) — they're real corrections you asked for, not drafts contingent on everything else also passing.

An existing file is edited in place with `toml_edit`, preserving comments, table order, and formatting; if the answer already matches what's there, nothing is written at all — the file's content and modification time are untouched. If the key already holds something other than a plain string, boolean, or integer (a table, an array, an array-of-tables, or an inline table), `am setup` refuses to overwrite it and reports an error instead, leaving the file byte-for-byte unchanged.

**Relationship to `am init` and `am doctor`**

`am init` stays the fast, silent, scriptable primitive — `am setup`'s first action is exactly what `am init` does, so the two share one implementation and cannot drift apart. `am setup`'s verification step *is* `am doctor`: it calls the same check logic and renders the same report, after any answers from steps 3–4 have been written — and the same shared `hint` text is what powers `am setup`'s "What to do next" block, so improving one improves both. Use `am init` when you already know what you want and would rather script it; use `am setup` when you want to be walked through it.

**Example output**

A run on a fresh repo, with `claude` credentials already present and no container runtime installed yet:

```
Setting up am for the git repository at /home/user/project
  Created .am/config.toml
  Added .am/worktrees/ to .gitignore
  Created ~/.config/am/config.toml

Which agent do you use?
  just this repo; saved to .am/config.toml.

  [1] claude    credentials found
  [2] copilot
  [3] gemini
  [4] codex

  currently: none configured

Agent [1-4] (Enter for claude): 

Use isolated containers for your sessions?
  every repo on this machine; saved to ~/.config/am/config.toml.

Each session gets its own isolated filesystem and process sandbox. Without containers, sessions run directly on the host.
  no container runtime was found on this machine yet — you can still opt in and install one before starting a session.

Use isolated containers for your sessions? [Y/n] 
Set defaults.agent = "claude" in .am/config.toml

Checking your setup...

[... the same report `am doctor` prints, ending in its verdict line ...]
Ready.

Which layout do you want?
  every repo on this machine; saved to ~/.config/am/config.toml.

  [1] agent left, 50/50
      [  agent   |  shell   ]
  [2] agent right, 50/50
      [  shell   |  agent   ]
  [3] agent left, 70/30
      [    agent    | shell ]
  [4] stacked, agent on top, 50/50
      [       agent        ]
      [       shell        ]
  [5] customize…

  currently: agent_pane=left, split=horizontal, split_percent=50 (am's default)

Layout [1-5] (Enter to keep current): 

Start your first session now? [Y/n] n

Next steps:
  am start feat --agent claude   # start your first session
  am doctor                      # re-check readiness any time
  am attach feat                 # jump back into a running session
```

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

**Example output**

A headline states the outcome, followed by the indented detail behind it. The detail lines are
dimmed, and the worktree path is shown relative to the repo root rather than as an absolute path:

```
Started session 'demo'
  worktree:  .am/worktrees/demo
  branch:    am/demo
  container: am-demo-7a2305
```

In devcontainer mode, an `image:` line follows `container:`. Outside of tmux — the `exec()` path
above — `branch:` and `image:` are omitted; that path only ever reports `worktree:` and
`container:`.

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

**Example output**

When the window is recreated for a session that has a container, a `Note:` call-out follows,
with a dimmed line underneath it showing how to restart cleanly:

```
Opened new window for session 'demo'.
  Note: the container was stopped when the window closed.
  To restart cleanly: am destroy --force demo && am start demo
```

The `Note:` line uses the same yellow severity as every other note in `am`. If the window already
exists, `am attach` just switches to it and prints `Attached to session '<slug>'.` with no detail.

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

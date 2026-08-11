# PRD: Global Session State

## Feature Overview

Move the session registry from a per-repo file (`<repo>/.am/sessions.json`) to a global
location (`$XDG_STATE_HOME/am/sessions.json`) so that `am` can list and manage sessions
from any working directory. As a by-product, `.am/config.toml` becomes committable because
the only non-committable artifact it contained (the sessions file) moves out.

**User value:**
- `am list` works from any directory, not just the repo where a session was started.
- Teams can commit `.am/config.toml` to share `agent`, `container`, and `devcontainer` defaults
  without each developer recreating them.
- Stale session records (from deleted or renamed repos) are surfaced and cleanable rather
  than silently rotting.

**Success criteria (how we know it is done):**
1. `am list` (scoped to current repo) returns the same output as before this change when run
   from the repo root.
2. `am list --all` returns sessions from all repos, with `[stale]` annotation for repos
   whose path no longer exists.
3. `am start`, `am destroy`, `am attach`, and `am run` all work without an
   `.am/sessions.json` file present.
4. `am init` no longer creates `.am/sessions.json` or `.am/gitconfig`; it writes only
   `.am/config.toml` and adds `.am/worktrees/` to `.gitignore`.
5. Running any `am` command against a repo that has an old-style `.am/sessions.json`
   migrates its records to the global store transparently and deletes the old file.
6. `am session rm <slug> --repo <path>` removes an orphaned record and attempts container
   and tmux cleanup.
7. Existing tests pass; new tests cover the global state path, migration, stale detection,
   and `am session rm`.

---

## Assumptions

1. **XDG_STATE_HOME default:** When `$XDG_STATE_HOME` is unset, `am` falls back to
   `~/.local/state` per the XDG Base Directory Specification. This parallels how
   `global_config_path()` already handles `$XDG_CONFIG_HOME`.
2. **sessions.json schema is additive:** Adding the `repo_root` field is backward-compatible
   because serde deserialises unknown fields without error, and old records without `repo_root`
   are treated as belonging to the repo where the migration finds them.
3. **Gitconfig generation at session start:** `am start` calls `git config get user.name`
   and `git config get user.email` (already done by the existing `read_git_config` helper)
   and writes a temporary gitconfig to a well-known path inside the global state directory
   rather than `.am/gitconfig`. This file is transient (not version-controlled).
4. **Migration is one-way:** Records are migrated from the old file to the global store;
   `am` never writes back to `.am/sessions.json` after migration.
5. **`am session rm` is destructive:** It removes the session record and makes a
   best-effort attempt to stop and remove the container and kill the tmux window. Errors in
   cleanup are logged as warnings, not failures — the record is always removed.
6. **`.am/worktrees/` in `.gitignore` is an additive change:** If `.am/` is already in
   `.gitignore`, `am init` still adds `.am/worktrees/` as the canonical entry. If both are
   present, the more-specific entry is harmless. The old broad `.am/` entry is left in place
   (removing it risks exposing already-ignored files to `git status`); an advisory message
   is printed if the old entry is detected.
7. **No global sessions.json pre-creation:** The global sessions file is created on first
   write (first `am start` or migration), not by `am init`. This matches how the config
   file works.
8. **`am session rm` without `--repo`:** If the slug is unique across all repos, it
   resolves unambiguously from anywhere. If the slug exists in multiple repos and the user
   is inside one of them, it resolves to that repo. Otherwise, `--repo` is required to
   disambiguate.
9. **Transient gitconfig path:** The generated gitconfig is written to
   `$XDG_STATE_HOME/am/gitconfig` (a single file shared across sessions, regenerated at
   each `am start`). It does not need to be per-session because its contents come from the
   user's global git config and do not change between sessions.

---

## Resolved Questions

1. **Gitconfig path for containers:** `$XDG_STATE_HOME/am/gitconfig` is the correct
   location. It is regenerated at each `am start` and is transient. The
   `ContainerMounts.gitconfig_host` default changes from `<repo>/.am/gitconfig` to this
   path.

2. **`am session rm` partial cleanup failure:** Best-effort: warn and proceed. The `--force`
   flag on `am session rm` skips the confirmation prompt only; cleanup failures are always
   warnings, never hard errors.

3. **Multi-session slug collision:** When the user is inside one of the repos with the
   conflicting slug, resolve to that repo's session (no error). When the user is outside
   all matching repos (or uses `--repo` pointing elsewhere), error and list the matching
   repos with a hint to use `--repo`.

4. **`am list --all` REPO column:** Abbreviate home directory as `~` (e.g.,
   `~/src/project` instead of `/home/user/src/project`). Full absolute paths are used
   internally; the abbreviation is display-only.

---

## Use-Cases

### UC-1: `am init` — initialize a repo (updated behavior)

**Actor:** Developer running `am` in a git or jj repo for the first time.

**Preconditions:** Current directory is inside a git or jj repo; `am` binary is on `$PATH`.

**Main flow:**
1. Developer runs `am init`.
2. `am` calls `find_repo_root()` to locate `<repo_root>`.
3. `am` creates `<repo_root>/.am/` if it does not exist.
4. If `<repo_root>/.am/config.toml` does not exist, `am` writes the default commented-out
   template (unchanged from current `write_defaults`). Prints `Created .am/config.toml`.
5. If `<repo_root>/.am/config.toml` already exists, prints `.am/config.toml already
   exists, skipping`.
6. `am` inspects `<repo_root>/.gitignore`:
   a. If `.am/worktrees/` or `.am/worktrees` is already present: prints
      `.am/worktrees/ already in .gitignore, skipping`.
   b. Otherwise: appends `.am/worktrees/` to `.gitignore`. Prints `Added .am/worktrees/ to
      .gitignore`.
   c. If `.am/` or `.am` is present (old-style): prints advisory `Note: .am/ is already in
      .gitignore; .am/config.toml is now committable — you may want to narrow this to
      .am/worktrees/ instead.`
7. Prints `am initialized. Run 'am start <slug>' to create your first session.`

**Alternative flows:**
- If `.gitignore` does not exist: create it and append `.am/worktrees/`.

**Exception flows:**
- File system error writing `.am/` or `.gitignore`: propagate as `IoError`; do not print
  success message.
- `find_repo_root()` returns `NotInRepo`: print `error: not in a git or jj repository`
  and exit non-zero.

**Postconditions:**
- `.am/config.toml` exists.
- `.am/worktrees/` is listed in `.gitignore`.
- `.am/sessions.json` does NOT exist (no longer created by `am init`).
- `.am/gitconfig` does NOT exist (no longer created by `am init`).

**Business rules:**
- `am init` is idempotent: safe to re-run in an already-initialized repo.
- The global sessions file (`$XDG_STATE_HOME/am/sessions.json`) is NOT created by `am
  init`; it is created lazily on first `am start`.

---

### UC-2: `am start <slug>` — start a session (updated behavior)

**Actor:** Developer inside an initialized repo.

**Preconditions:**
- Current directory resolves to a repo root.
- `.am/config.toml` may or may not exist (optional; defaults apply if absent).
- Global sessions file may or may not exist (created lazily).

**Main flow:**
1. `am start feat` resolves `<repo_root>` and `vcs` via `find_repo_root()`.
2. Load sessions from the global store: `load_sessions_global()` reads
   `$XDG_STATE_HOME/am/sessions.json`.
3. **Migration check:** if `<repo_root>/.am/sessions.json` exists, run
   `migrate_sessions(repo_root)` (see UC-6) before proceeding.
4. Filter sessions by `repo_root` to check for slug collision.
5. If `feat` already exists in the global store for this `repo_root`: return
   `SlugAlreadyExists("feat")`.
6. Load config (`config::load_with_global`).
7. **Gitconfig generation:** call `read_git_config("user.name")` and
   `read_git_config("user.email")`; write the result to
   `$XDG_STATE_HOME/am/gitconfig` as:
   ```
   [user]
       name = <name>
       email = <email>
   ```
   If both values are empty, write an empty `[user]` block (not an error; git behaves the
   same).
8. If containers are enabled and `container.gitconfig` is not set, the mount source for
   gitconfig is now `$XDG_STATE_HOME/am/gitconfig` (not `<repo_root>/.am/gitconfig`).
9. Complete the existing container/tmux/worktree flow unchanged.
10. Build a `Session` record with the new `repo_root` field set to the absolute
    `<repo_root>` path.
11. Write the session record to the global store: `add_session_global(session)`.

**Gitconfig path check (replaces old `.am/gitconfig` existence check):**
- Step 2 of the existing preflight check currently errors when `.am/gitconfig` does not
  exist and `container.gitconfig` is not set. After this change, `am start` generates the
  gitconfig unconditionally at step 7, so this preflight check is removed entirely. If
  `$XDG_STATE_HOME/am/` cannot be created, an `IoError` is returned before any side
  effects.

**Alternative flows:**
- If `container.gitconfig` is explicitly set: use that path (unchanged); skip step 7's
  gitconfig generation (or generate anyway — it is cheap and harmless).

**Exception flows:**
- `find_repo_root()` fails: `NotInRepo` error.
- `$XDG_STATE_HOME` cannot be determined (no `HOME`): return a clear error `cannot
  determine XDG_STATE_HOME: HOME is not set`.
- `git config get user.name` fails or returns empty: write the gitconfig with an empty
  name/email; do not abort.
- Global sessions file cannot be written: return `IoError`.

**Postconditions:**
- Session record exists in `$XDG_STATE_HOME/am/sessions.json` with `repo_root` = absolute
  path of the repo.
- `$XDG_STATE_HOME/am/gitconfig` reflects the current user's git identity.
- Worktree, tmux window, and container are created as before.

**Business rules:**
- Slug uniqueness is scoped to `repo_root`: the same slug may exist in two different
  repos' sessions without conflict.
- The session record's `repo_root` field must be an absolute, canonical path (resolved via
  `std::fs::canonicalize` or equivalent).

---

### UC-3: `am list` — list sessions for current repo (updated behavior)

**Actor:** Developer inside a repo.

**Preconditions:** Current directory is inside a git or jj repo.

**Main flow:**
1. `am list` calls `find_repo_root()`.
2. **Migration check:** if `<repo_root>/.am/sessions.json` exists, run `migrate_sessions`.
3. Load all sessions from global store.
4. Filter to sessions where `session.repo_root == <repo_root>`.
5. If no sessions remain after filtering: print `No active sessions. Run 'am start
   <slug>' to begin.`
6. Otherwise print the existing tabular output (SLUG, CONTAINER, AUTO, WORKTREE, WINDOW,
   CREATED — unchanged columns).

**Alternative flows:**
- `am list --all`: skip `find_repo_root()`; load all sessions from global store; show all
  records regardless of `repo_root`. Add a REPO column (see UC-4).

**Exception flows:**
- `find_repo_root()` fails when `--all` is NOT passed: return `NotInRepo` error.
- `find_repo_root()` fails when `--all` IS passed: proceed without filtering — `--all`
  does not require a repo context.

**Postconditions:** None (read-only).

**Business rules:**
- `am list` (no flag) without an `--all` flag requires a repo context.
- `am list` never shows stale records in the default (scoped) view because stale means
  the `repo_root` no longer exists; if the user is running `am` from within the repo, it
  exists by definition.

---

### UC-4: `am list --all` — list sessions across all repos

**Actor:** Developer, possibly outside any repo.

**Preconditions:** None — can be run from any directory.

**Main flow:**
1. Load all sessions from global store.
2. For each session, check if `session.repo_root` path exists on disk.
   - If it does not exist: mark session as stale.
3. Sort sessions: non-stale first (by `repo_root` alphabetically, then by `created_at`),
   stale last.
4. Print tabular output with columns: REPO, SLUG, CONTAINER, AUTO, WORKTREE, WINDOW,
   CREATED, STATUS.
   - STATUS is blank for live sessions; `stale` for sessions whose `repo_root` is gone.
   - REPO shows the basename of `repo_root` by default; full path when the terminal is wide
     enough or a `--wide` flag is supplied. (Implementation note: for the initial
     implementation, always show full path and accept that the table may wrap on narrow
     terminals.)
5. If no sessions in global store: print `No sessions found across any repo.`

**Exception flows:**
- Global sessions file does not exist: treat as empty, print the empty message.

**Postconditions:** None.

**Business rules:**
- A session is stale when `std::path::Path::exists(session.repo_root)` returns `false`.
- Stale sessions are shown but not automatically removed; the user must run `am session rm`
  explicitly.

---

### UC-5: `am session rm <slug>` — remove a session record

**Actor:** Developer cleaning up stale or orphaned session records.

**Preconditions:** A session with the given slug exists in the global store, either for the
current repo or (with `--repo`) for a specified repo.

**Main flow:**
1. Developer runs `am session rm feat` from inside a repo, or
   `am session rm feat --repo /path/to/repo` from anywhere.
2. Resolve the repo root:
   - With `--repo <path>`: use the provided path (validate it is absolute or resolve it to
     absolute; the path need not exist on disk — this command is for cleanup).
   - Without `--repo`: call `find_repo_root()` as usual.
3. Load all sessions from global store.
4. Find session with matching slug and `repo_root`. If not found: return
   `SlugNotFound("feat")`.
5. **Confirmation prompt** (unless `--force`):
   ```
   Remove session 'feat' from <repo_root>? [y/N]
   ```
   If user enters anything other than `y`/`Y`: print `Aborted.` and exit 0.
6. **Best-effort cleanup** (errors logged as warnings, not failures):
   a. If `session.container` is set: attempt `stop_container` and `remove_container` using
      the recorded runtime. Log any error as `warning: container cleanup failed: <err>`.
   b. If tmux window name is known: attempt `kill_window(session.tmux.tmux_window)`. Log
      any error as `warning: tmux cleanup failed: <err>`.
   c. Note: no worktree removal is attempted — `am session rm` is for orphaned records
      where the worktree may already be gone. If the user wants worktree removal, they
      should use `am destroy` instead.
7. Remove the session record from the global store: `remove_session_global(repo_root, slug)`.
8. Print `Removed session 'feat'.`

**Alternative flows:**
- `--force`: skip the confirmation prompt; proceed directly to step 6.
- Stale session (repo no longer exists): proceed normally — the record is still removable
  even when `repo_root` is gone.

**Exception flows:**
- Session not found: `SlugNotFound("feat")`.
- Global sessions file write fails: `IoError`.
- `find_repo_root()` fails (no repo context, no `--repo`): `NotInRepo`.

**Postconditions:**
- Session record is removed from the global store.
- Container and tmux window are stopped/removed on a best-effort basis.

**Business rules:**
- `am session rm` does NOT remove the git worktree or jj workspace. It is for registry
  cleanup only. If the worktree is still present, the user should use `am destroy` instead.
- If multiple sessions share the same slug across different repos (possible in the global
  store) and `--repo` is not given:
  a. If the user is inside one of the matching repos (i.e., `find_repo_root()` succeeds
     and matches one of the sessions' `repo_root` values): resolve to that repo's session
     without error.
  b. Otherwise, print:
     ```
     error: slug 'feat' exists in multiple repos:
       /path/to/repo-a
       /path/to/repo-b
     Use --repo <path> to specify which one.
     ```
     and exit non-zero.

---

### UC-6: Migration — transparent upgrade from per-repo sessions

**Actor:** `am` (automatic, triggered by any command that loads sessions for a specific repo).

**Preconditions:** `<repo_root>/.am/sessions.json` exists (old-style layout).

**Main flow (called from `load_sessions_for_repo` or before any session read):**
1. Detect `<repo_root>/.am/sessions.json`.
2. Parse the old file's `sessions` array.
3. For each session record, add the `repo_root` field (set to the current `<repo_root>`).
4. Merge into the global store: for each record, if a session with the same `repo_root` +
   `slug` does not already exist, append it; if it does exist, skip (no overwrite).
5. Delete `<repo_root>/.am/sessions.json`.
6. Print to stderr: `Migrated N session(s) from .am/sessions.json to global store.`
   (Use stderr so the message does not break scripts parsing `am list` stdout.)

**Exception flows:**
- Old file is malformed JSON: print a warning to stderr (`warning: could not parse
  .am/sessions.json for migration: <err> — leaving file in place`); do NOT delete the old
  file; do NOT abort the command in progress.
- Global store write fails: propagate `IoError`; do NOT delete the old file (leave it for
  the next attempt).
- Old file exists but is empty or has `{"sessions":[]}`: migrate zero records; delete the
  old file silently.

**Postconditions:**
- `<repo_root>/.am/sessions.json` no longer exists (unless migration failed).
- All former local sessions appear in the global store with `repo_root` set.

**Business rules:**
- Migration is idempotent: re-running against the same old file (if it was not deleted due
  to a write error) must not create duplicate records.
- Migration must not block the command that triggered it. If the global store cannot be
  written, the command should still attempt to proceed using the old file's data for the
  current invocation (best-effort degraded mode). This avoids a scenario where a write
  failure causes `am start` or `am list` to fail completely.

---

## Data Model

### New field on `Session`

```rust
pub struct Session {
    pub slug: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub auto: bool,
    /// Absolute path of the repository this session belongs to.
    /// Added for global state; records without this field (migrated from the old per-repo
    /// file) must have it populated by the migration path before writing to the global store.
    pub repo_root: PathBuf,
    #[serde(flatten)]
    pub vcs: VcsMetadata,
    #[serde(flatten)]
    pub tmux: TmuxMetadata,
    pub container: Option<SessionContainer>,
}
```

`repo_root` uses `#[serde(default)]` is **not** appropriate here — every record in the
global store must have `repo_root`. However, for backward-compatibility when reading old
per-repo files during migration, parse into a temporary struct that allows a missing
`repo_root`, then populate it before writing to the global store.

### Global sessions file location

```
$XDG_STATE_HOME/am/sessions.json      (when XDG_STATE_HOME is set)
~/.local/state/am/sessions.json       (fallback)
```

Helper function (analogous to `config::global_config_path()`):

```rust
pub fn global_sessions_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("am").join("sessions.json"))
}
```

### Global gitconfig location

```
$XDG_STATE_HOME/am/gitconfig
~/.local/state/am/gitconfig
```

This file is regenerated at each `am start` and is transient (never version-controlled).

### Uniqueness constraint

Within the global sessions file, the pair `(repo_root, slug)` must be unique. The
`add_session_global` function enforces this.

### Indexes needed

No database — JSON file is small. However, the session-loading functions that filter by
`repo_root` iterate linearly over all records, which is acceptable for the expected scale
(tens of sessions per machine, not thousands).

---

## API Design (internal Rust API)

These are the function signatures the engineer must implement or update. There is no HTTP
API. The session module is the primary touch point.

### New functions in `session.rs`

```rust
/// Resolve the global sessions file path.
/// Returns None only if neither XDG_STATE_HOME nor HOME is set.
pub fn global_sessions_path() -> Option<PathBuf>

/// Load all sessions from the global store.
pub fn load_all_sessions() -> Result<Vec<Session>>

/// Load sessions belonging to a specific repo (filters by repo_root).
/// Returns an empty Vec if the global file does not exist yet.
pub fn load_sessions_for_repo(repo_root: &Path) -> Result<Vec<Session>>

/// Add a session to the global store.
/// Errors with SlugAlreadyExists if (repo_root, slug) already exists.
pub fn add_session_global(session: Session) -> Result<()>

/// Remove a session from the global store by (repo_root, slug).
/// Errors with SlugNotFound if not found.
pub fn remove_session_global(repo_root: &Path, slug: &str) -> Result<()>

/// Migrate sessions from an old per-repo file into the global store.
/// Deletes the old file on success. Idempotent.
/// Returns the number of records migrated (0 if none or already migrated).
pub fn migrate_sessions(repo_root: &Path) -> Result<usize>
```

### Deprecated / removed functions in `session.rs`

The existing `load_sessions(repo_root)`, `save_sessions(repo_root, sessions)`, and
`add_session(repo_root, session)` operate on the per-repo file. These are replaced by the
global equivalents above. The old functions should be retained temporarily with a
`#[deprecated]` attribute and removed once all call sites are updated. `remove_session`
becomes `remove_session_global`.

### Changes to `config.rs`

Add a new public function (symmetric with `global_config_path`):

```rust
/// Returns the global state directory: $XDG_STATE_HOME/am or ~/.local/state/am.
pub fn global_state_dir() -> Option<PathBuf>
```

This is used by both `session.rs` (for `sessions.json`) and `main.rs` (for gitconfig).

### Changes to `main.rs`

**`cmd_init`:** Remove the blocks that create `sessions.json` and `gitconfig`. Update
`.gitignore` logic to target `.am/worktrees/` instead of `.am/`.

**`cmd_start`:**
- Replace `session::load_sessions(&repo_root)` with `session::load_sessions_for_repo(&repo_root)`.
- Add migration check before the slug collision check.
- Remove the `.am/gitconfig` existence check.
- Add gitconfig generation step (write to `global_state_dir()/gitconfig`).
- Update the gitconfig mount path used in `plan_image` / `plan_devcontainer`.
- Replace `session::add_session(&repo_root, new_session)` with
  `session::add_session_global(new_session)` (where `new_session` now carries `repo_root`).

**`cmd_list`:**
- Add `--all` flag (boolean) to the `List` variant in `cli.rs`.
- If `--all`: call `session::load_all_sessions()`; skip `find_repo_root()`.
- Otherwise: call `session::load_sessions_for_repo(&repo_root)` after migration check.
- Add STATUS column to `--all` output.

**`cmd_destroy`:**
- Replace `session::load_sessions(&repo_root)` with `session::load_sessions_for_repo`.
- Replace `session::remove_session(&repo_root, slug)` with `session::remove_session_global`.

**`cmd_attach` and `cmd_run`:**
- Same load/find replacement as `cmd_destroy`.

**New `cmd_session_rm`:** Implement the `am session rm` handler.

### Changes to `cli.rs`

```rust
#[derive(Subcommand)]
pub enum Commands {
    // ... existing variants unchanged ...

    /// List sessions (default: current repo; --all shows all repos)
    List {
        #[arg(long, help = "Show sessions from all repos")]
        all: bool,
    },

    /// Manage session records in the global store
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
}

#[derive(Subcommand)]
pub enum SessionCommands {
    /// Remove a session record (and attempt container/tmux cleanup)
    Rm {
        #[arg(value_parser = validate_slug)]
        slug: String,
        /// Repository root containing this session (required when not inside a repo,
        /// or when the slug exists in multiple repos)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },
}
```

Note: `List` changes from a unit variant to a struct variant, which changes how it is
matched in `run()`. All callers must be updated.

---

## Config Layering and Committable `.am/config.toml`

With `.am/config.toml` now committable, the existing config precedence order is important:

```
defaults → global config → project config → env vars → CLI flags
```

This is the conventional "most-specific wins" pattern (same as git). A team commits
project-level settings (e.g., `agent`, `container.mode`, `devcontainer` options) in
`.am/config.toml`. Individual developers set personal preferences (e.g.,
`container.runtime`, `container.ssh`) in their global `~/.config/am/config.toml`. If a
user needs to override a committed project setting, env vars (`AM_*`) and CLI flags serve
as the escape hatch — no config file changes required.

### Removal of `vcs` config setting

The `vcs` field in `[defaults]` is removed from the config schema entirely.
`find_repo_root()` already auto-detects VCS by checking for `.jj/` (preferred) then
`.git/` on disk. The config value was parsed but never consumed by any command — it is
dead code. Removing it avoids the problem of a committed config forcing a VCS choice that
doesn't match a developer's actual setup (e.g., a jj user cloning a repo whose committed
config says `vcs = "git"`).

**Changes:**
- Remove `vcs` from `Config`, `FileConfig`, `Vcs` enum references in config parsing
- Remove the `vcs` line from `write_defaults()` and `global_config_template()`
- Remove the `AM_VCS` env var handling if present
- Keep `Vcs` enum itself (used by `find_repo_root()` return type and worktree operations)

---

## Implementation Tasks

### backend-engineer

- [ ] **`config.rs`:** Remove `vcs` from `Config` struct, `FileConfig` struct,
  `apply_file_config`, `write_defaults`, and `global_config_template`. Keep the `Vcs` enum
  (still used by `find_repo_root` and worktree operations).
- [ ] **`config.rs`:** Add `global_state_dir() -> Option<PathBuf>` using
  `$XDG_STATE_HOME` (falling back to `~/.local/state`). This is the single source of
  truth for the state directory location.
- [ ] **`session.rs`:** Add `repo_root: PathBuf` to `Session`. Use `#[serde(default)]`
  on a temporary migration-only struct (not on `Session` itself) so that records without
  `repo_root` can be read during migration and then written with it populated.
- [ ] **`session.rs`:** Implement `global_sessions_path()`.
- [ ] **`session.rs`:** Implement `load_all_sessions()`, `load_sessions_for_repo()`,
  `add_session_global()`, `remove_session_global()`, `migrate_sessions()`. Follow the
  same file-write pattern as existing `save_sessions` (pretty-print JSON, atomic where
  possible).
- [ ] **`session.rs`:** Mark old `load_sessions`, `save_sessions`, `add_session`,
  `remove_session` as `#[deprecated]`.
- [ ] **`main.rs` — `cmd_init`:** Remove `sessions.json` creation. Remove `gitconfig`
  creation. Change `.gitignore` logic: target `.am/worktrees/`, detect old `.am/` entry
  and print advisory.
- [ ] **`main.rs` — `cmd_start`:** Add migration call. Remove `.am/gitconfig` preflight
  check. Add gitconfig generation step writing to `global_state_dir()/gitconfig`. Update
  gitconfig mount source. Switch to `load_sessions_for_repo` and `add_session_global`.
  Set `repo_root` on the new `Session` record.
- [ ] **`main.rs` — `cmd_list`:** Handle new `List { all }` variant. Branch on `all` for
  load and display. Add REPO and STATUS columns for `--all` output. Add stale detection.
- [ ] **`main.rs` — `cmd_destroy`:** Switch to `load_sessions_for_repo` and
  `remove_session_global`.
- [ ] **`main.rs` — `cmd_attach` / `cmd_run`:** Switch to `load_sessions_for_repo`.
- [ ] **`main.rs` — `cmd_session_rm`:** Implement new handler (UC-5). Add migration check.
- [ ] **`cli.rs`:** Convert `List` from unit to struct variant. Add `Session`/`SessionCommands`
  tree. Wire new commands in `run()`.
- [ ] **`container.rs` / `plan_image` / `plan_devcontainer`:** Update gitconfig mount
  source to use `global_state_dir()/gitconfig` when `container.gitconfig` is not set.
- [ ] **`error.rs`:** Add `GlobalStateDirNotFound` variant for when neither
  `XDG_STATE_HOME` nor `HOME` is set. Message:
  `"cannot determine state directory: neither XDG_STATE_HOME nor HOME is set"`.

### integration-tester

- [ ] Test that `vcs` is no longer accepted in config files (or is silently ignored).
- [ ] Test `am init` no longer creates `sessions.json` or `gitconfig`.
- [ ] Test `am init` writes `.am/worktrees/` (not `.am/`) to `.gitignore`.
- [ ] Test `am init` prints advisory when old `.am/` entry already exists.
- [ ] Test `am start` creates session in `$XDG_STATE_HOME/am/sessions.json`.
- [ ] Test `am start` session record has `repo_root` set to the canonical repo root path.
- [ ] Test `am start` generates `$XDG_STATE_HOME/am/gitconfig`.
- [ ] Test `am list` returns only sessions for the current repo.
- [ ] Test `am list --all` returns sessions from all repos, including stale.
- [ ] Test `am list --all` marks session as stale when `repo_root` does not exist.
- [ ] Test migration: running any command against a repo with `.am/sessions.json` migrates
  records to global store and deletes the old file.
- [ ] Test migration idempotency: re-running when old file is gone does nothing.
- [ ] Test migration with malformed old file: old file is left in place, command proceeds.
- [ ] Test `am session rm feat` removes record from global store.
- [ ] Test `am session rm feat --force` skips confirmation.
- [ ] Test `am session rm feat --repo /path` works outside a repo.
- [ ] Test `am session rm` with slug matching two repos errors and names both.
- [ ] Test `am destroy` still works after migration.
- [ ] Test `am attach`, `am run` still work after migration.
- [ ] Test slug uniqueness is per-repo (same slug in two repos is allowed).
- [ ] Unit tests for `global_sessions_path()` with and without `XDG_STATE_HOME`.
- [ ] Unit tests for `migrate_sessions()` — zero records, non-zero records, duplicate guard.
- [ ] Unit tests for `load_sessions_for_repo()` filtering.
- [ ] Unit tests for `add_session_global()` — uniqueness enforcement on `(repo_root, slug)`.

### code-reviewer

- [ ] Verify `repo_root` is always stored as a canonical absolute path (not relative, not
  with trailing slash).
- [ ] Verify migration deletes the old file only after a successful global store write.
- [ ] Verify `am session rm` leaves the worktree untouched.
- [ ] Verify the stale check uses `Path::exists()` (not `metadata()`) for consistency.
- [ ] Verify `cmd_list --all` does not call `find_repo_root()` (would fail outside a repo).
- [ ] Verify `global_state_dir()` is the single source of truth — no other code constructs
  `XDG_STATE_HOME` paths independently.
- [ ] Verify old deprecated functions are not called anywhere after migration.
- [ ] Check that the `(repo_root, slug)` uniqueness invariant is enforced at write time,
  not just at session-start time (i.e., `add_session_global` checks, not only `cmd_start`).

### documentation-writer

- [ ] Update `docs/getting-started/` to remove the reference to `.am/` being gitignored
  and add a note that `.am/config.toml` is now committable.
- [ ] Update `docs/reference/` (config reference) to note that `sessions.json` has moved.
- [ ] Add a new reference page or expand the existing one to document `am session rm` and
  `am list --all`.
- [ ] Update `SPEC.md` to reflect the new `.am/` directory layout (what is present,
  what is absent).
- [ ] Update `PLAN.md` to add this feature with its sub-tasks.
- [ ] Update inline comments in `write_defaults` (the `config.toml` template) to remove
  the note about `.am/sessions.json`, if any exists.

---

## Edge Cases and Considerations

### Security

- **`--repo <path>` in `am session rm`:** The provided path is used only for record
  lookup, not for file operations against that path (the worktree is not touched). No path
  traversal risk beyond what `find_repo_root()` already accepts.
- **Global sessions file permissions:** Created with the user's `umask` (same as any other
  file written by `am`). No special permissions needed — it contains only metadata about
  the user's own sessions.
- **Gitconfig at `$XDG_STATE_HOME/am/gitconfig`:** Contains the user's git identity
  (name, email). This is not sensitive but should not be world-readable on multi-user
  systems. The file is written with the user's `umask`; no explicit chmod is needed.

### Performance

- The global sessions file is read on every `am` invocation that involves session state.
  At the expected scale (dozens of sessions), a full linear scan and JSON parse is
  negligible. No caching or indexing is needed.

### Race conditions

- Two simultaneous `am start` commands in the same repo with the same slug will both pass
  the initial slug check and then race to write the global store. The last write wins.
  This is the same race that exists in the current per-repo sessions file. Fixing it
  requires file locking, which is out of scope for this feature. Document as a known
  limitation.

### UX — error messages

- When `global_sessions_path()` returns `None` (no `HOME`): `error: cannot determine
  state directory: neither XDG_STATE_HOME nor HOME is set`.
- When a user runs `am list` outside a repo without `--all`: `error: not in a git or jj
  repository. Run 'am list --all' to see sessions from all repos.` (augment the existing
  `NotInRepo` message in this context, or catch the error in `cmd_list` and re-wrap it).
- When `am session rm` finds the slug in multiple repos: list both paths and instruct use
  of `--repo`.

### Backward compatibility

- Old per-repo `sessions.json` files are migrated automatically; no user action required.
- Sessions recorded before this change have no `repo_root` field. The migration assigns
  `repo_root` from the repo where the file is found, which is correct.
- If a user downgrades `am` after migration, `am list` returns an empty list (the old
  `load_sessions` reads `<repo>/.am/sessions.json`, which no longer exists). Sessions are
  not lost — they remain in the global file. Upgrading again restores visibility.

### Scalability

- One global file per machine. If a developer has 50 repos each with 10 sessions, the
  global file has 500 records (~100 KB). JSON round-trip at this scale is well under 1 ms.
  No concern.

### Testing — environment isolation

- Tests that write sessions must inject `XDG_STATE_HOME` via `std::env::set_var` and hold
  the `ENV_MUTEX` (already used in `config.rs` tests) to prevent cross-test pollution.
- Tests that exercise `global_sessions_path()` must save and restore `XDG_STATE_HOME` and
  `HOME` using the existing `EnvGuard` pattern.

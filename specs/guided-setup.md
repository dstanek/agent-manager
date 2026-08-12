# Feature: Guided Setup (`am setup`)

A new, interactive command that walks a first-time or new-repo user through configuring
`am`, asking only the questions that detected state can't answer on its own, then verifying
the result and optionally launching a first session.

## Background

Today the on-ramp is `am init`: it creates `.am/config.toml` (fully commented out),
appends `.am/worktrees/` to `.gitignore`, and prints one line. It asks nothing and assumes
the user already knows what `agent`, `container.runtime`, and `[agents.<name>].image` mean.
`am doctor` fills the *diagnostic* half of the gap — it tells you precisely what's missing —
but it changes nothing and a first-time user still has to translate "no container runtime
found" into "go install Podman" and "no image configured for the selected agent" into
editing TOML by hand.

The user's own goal — "make using `am` as easy as possible" — is a UX gap between these two
existing commands, not a hole in functionality. `am setup` fills it.

## Assumptions

- The target user already has `am` installed and is running it from inside a git or jj
  repository. `am setup` does not run `git init` on their behalf, the same way `am init`
  doesn't today.
- "As easy as possible" means *fewer decisions*, not *more dialog*. Every question below has
  to earn its place by being something detected state genuinely cannot answer.
- Users already comfortable with `am` keep using `am init` for new repos, `am doctor` to
  debug, and hand-editing `.am/config.toml` for anything outside the two settings `am setup`
  knows how to ask about. `am setup` doesn't replace those workflows or grow into a general
  config editor — see [Resolved Decisions](#resolved-decisions) #4.
- No new prompt-UI dependency is worth adding for this; a format-preserving TOML-edit
  dependency is (see #2 and #5 below, and [What it writes](#what-it-writes)).

## Use cases

### UC1 — Brand-new user, first repo, no config anywhere

**Actor:** a user who has just installed `am` and has never run any `am` command.
**Preconditions:** inside a git or jj repo. Neither `.am/config.toml` nor
`~/.config/am/config.toml` exists.
**Main flow:**

1. User runs `am setup`.
2. `am setup` detects repo/VCS, writes a fresh `.am/config.toml` skeleton and the
   `.gitignore` entry (the same work `am init` does today — see
   [Scope boundary](#scope-boundary-vs-am-init-and-am-doctor)), and creates a fresh
   `~/.config/am/config.toml` skeleton.
3. The agent question is asked — every prompt shows a default derived from the *effective*
   current value, and here nothing is configured anywhere, so the default is "none" (see
   [Question flow](#question-flow)). Answering writes it into the project file.
4. The containers question is asked only if no runtime is currently found on the host; if
   answered, it's written into the global file.
5. `am setup` runs the same checks `am doctor` runs and prints the report inline.
6. If the report is clean, `am setup` offers to start a first session.
7. User accepts, provides a slug, `am setup` calls the same code path as `am start`.

**Postcondition:** `.am/config.toml` and `~/.config/am/config.toml` exist with the user's
choices active; a running session may exist if the user opted in.

### UC2 — Existing user, adding `am` to a new repo

**Actor:** a user with a working global config from a previous repo.
**Preconditions:** inside a repo with no `.am/config.toml`. `~/.config/am/config.toml`
already exists and sets an agent.
**Main flow:**

1. User runs `am setup`.
2. A fresh project config skeleton is created (as UC1 step 2); the existing global config is
   opened, not recreated.
3. The agent question is asked with its default sourced from the **global** config
   ("agent: claude *(from your global config)*"). Pressing Enter accepts it — and because the
   answer didn't change, nothing is written to the project file, which keeps its `agent` line
   commented and continues to inherit from global, exactly as if the question had been
   skipped. Typing a different agent instead writes an explicit override into the project
   file only.
4. The containers question is asked only if the trigger condition currently holds (no
   runtime found); if the global config already resolved this in a previous run, the default
   shown reflects that, and accepting it writes nothing.
5. Verification runs and prints; offer to start a first session, as UC1.

This is the common case after the very first repo. With a healthy global config and a
detected runtime, it now involves exactly one real question (agent) that a single Enter
press answers correctly.

### UC3 — `am start` failed, user wants to be walked through fixing it

**Actor:** a user who ran `am start` (or `am doctor`) and got a failure they don't know how
to resolve.
**Preconditions:** `.am/config.toml` already exists (whether or not it's correctly
configured) — e.g. it names an agent whose credentials aren't on this host, or containers
are disabled with no runtime installed.
**Main flow:**

1. User runs `am setup`.
2. Project config already exists, so it is **not** recreated — it's opened, and its current
   `agent` value (if any) becomes the shown default, labeled `(from this project's config)`.
3. The user can now genuinely fix the problem here: e.g. switch from `agent = "codex"`
   (no `OPENAI_API_KEY` set) to `agent = "claude"` (credentials already present on this
   host) by typing a new answer. This is the one place `am setup` can change an existing
   choice, not just re-report it.
4. If no runtime is currently found, the containers question offers to disable containers so
   the user can keep working without one, or the user can leave it as-is and go install one.
5. `doctor::run()` re-verifies against whatever is now in the (possibly just-updated) config
   and prints the report — reflecting any change just made.
6. Clean → offer a first session. Not clean → the report's own hints are the guidance;
   `am setup` stops there.

**Scope boundary, restated for the revised flow:** `am setup` can now change the two things
it already knows how to ask about — which agent, and whether containers are enabled — on an
existing config. It still does **not** become a general repair tool: it never installs a
container runtime, never runs `gh auth login` or writes a token, and never edits any config
key outside `defaults.agent` and `container.enabled`. That boundary is unchanged in kind; the
[OQ5 override](#resolved-decisions) only moved "agent" from "fixed once written" to "always
revisitable," it didn't add new categories of automated repair.

## Scope boundary vs. `am init` and `am doctor`

**A new, separate command — `am setup` — not a flag on `init`.** `am init` must stay fast,
silent, and scriptable: it's invoked from the cucumber fixtures and (per the docs) from setup
scripts as an idempotent, non-interactive primitive, and making it interactive by default
inverts that contract for every existing caller. A flag (`am init --interactive`) also buries
the feature — the whole point is discoverability for someone who doesn't know what to type
yet, and a bare, guessable verb serves that better than a flag. This also matches the
"fast path vs. custom path" split BACKLOG.md already calls for: `am init` (fast, scriptable)
and `am setup` (guided, interactive) become the two doors.

**`am setup` is additive, not a rewrite:** its first action is running the exact same logic
`am init` runs (`.am/` + `.gitignore`), extracted into a shared helper so `cmd_init` and
`cmd_setup` cannot drift apart — the same "don't let a passing check and a working command
disagree" principle `am doctor` already follows for `cmd_start`'s preflight functions.

**Relationship to `am doctor`: `am setup`'s verification step *is* `doctor::run()`.** No new
check logic is written. `am setup` computes what's already configured using the same
primitives `doctor::run()` and `cmd_start` already call (`container::detect_runtime`,
`container::validate_agent_credentials`, `devcontainer::find_config`, `tmux::find_tmux`), and
after writing config it calls `doctor::run()` for real and renders it with the existing
`Report::render()` — byte-for-byte the same output `am doctor` would print for that repo.

This does **not** reverse the stance BACKLOG.md records for `am doctor` ("the alternative to
auto-bootstrapping `.am/` as a side effect of `am start`"). `am start` still does not
auto-bootstrap anything. `am setup` is an explicit, user-invoked command whose entire job
*is* bootstrapping — it's the opt-in front door, not a side effect.

## Question flow

The guiding rule is unchanged: **ask only what detected state can't answer.** What changed
(per [Resolved Decision #5](#resolved-decisions)) is what "answer" means when a file already
has a value — the question is still asked (gated exactly as before), but its default is now
the *effective current value*, shown with where it comes from, and accepting that default is
a guaranteed no-op.

```
1. Preconditions (no prompt)
   ├─ not in a repo?              → same error as `am init`, exit
   └─ in a repo                   → proceed, VCS (git/jj) detected silently

2. Project config file (no prompt — an action, not a question)
   ├─ .am/config.toml missing     → write skeleton + .gitignore entry (= `am init`)
   └─ .am/config.toml exists      → open it, don't touch it yet — its values feed step 3

3. Global config file (no prompt — an action, not a question)
   ├─ ~/.config/am/config.toml missing  → write skeleton
   └─ ~/.config/am/config.toml exists   → open it, don't touch it yet — feeds steps 3-4

4. Agent question — ALWAYS asked, unless --agent or --yes was passed
   "Which agent do you use? [1] claude  [2] copilot  [3] gemini  [4] codex"
   Default shown = the effective value read via project config → global config → compiled
   default (none), labeled with its source, e.g.:
     "agent: claude (from your global config)"
     "agent: codex (from this project's config)"
     "agent: none configured"
   If nothing sets an effective value yet, the menu's pre-selected option is instead the
   first agent with credentials already detected on this host (same probe
   `validate_agent_credentials` uses), falling back to "claude" — this is the only case
   where the shown default isn't literally "what's already configured," because nothing is.
   Accepting the default (Enter, or an --yes/--agent value equal to it) writes NOTHING.
   A genuinely different answer writes `defaults.agent` into the PROJECT file only — never
   the global file, regardless of where the shown default came from (see
   "What it writes" below for why).

5. Containers question — asked ONLY if neither podman nor docker is currently on PATH
   "No container runtime found on this machine. Proceed with containers disabled for now?
   [y/N]"
   Default shown = the effective `container.enabled` value read from the GLOBAL config
   (project-level overrides of this key are intentionally out of scope for this question —
   see Resolved Decision #4), labeled with its source.
   Accepting the default writes nothing. A changed answer writes `container.enabled` into
   the GLOBAL file only.
   If a runtime IS found, this question is not asked at all, regardless of what's already
   configured — see the edge case on stale disables below.

6. Project-specific notes (no prompt — informational only)
   ├─ .devcontainer/devcontainer.json found → one line: "found — sessions will use it
   │    automatically (container.mode = auto)". No question: auto is already the correct
   │    default, and asking "use your devcontainer?" when the answer is obviously yes
   │    fails the "every question must justify itself" bar.
   │    If it also contains `initializeCommand` (which `am` refuses by default): a warning
   │    line pointing at `am doctor` and `devcontainer.allow_host_commands`, NOT a prompt —
   │    silently enabling host command execution from a wizard default is not acceptable.
   └─ none found                 → nothing printed; image mode is already the default

7. Verification (no prompt)
   → run doctor::run() against the resolved repo + agent (reflecting whatever was just
     written in steps 4-5), render exactly as `am doctor`
   → 0 failures  → continue to step 8
   → failures>0  → print the report, exit 1 (see UC3); step 8 is not reached

8. First session (prompt only if step 7 passed AND session is interactive, i.e. not --yes
   and stdin is a TTY)
   → "Start your first session now? [Y/n]"
        no / declined  → print next-step commands, exit 0
        yes            → "Session name: " (no default — an empty answer re-prompts once,
                           then falls back to declining and printing next steps rather than
                           looping forever on a required field)
                        → calls the same function `cmd_start` uses, with the resolved agent
```

**Empty input:** accepts the shown default. For the agent menu, empty input accepts the
marked default option.

**Invalid input** (an out-of-range menu number, or `y`/`n`/blank being none of those): the
question is re-asked with a one-line reason; there's no retry limit because the underlying
validation is trivial and an infinite loop here is no worse than a normal shell prompt
rejecting bad input.

**Ctrl-C / SIGINT:** default process behavior (immediate exit). Both config writes (steps 4
and 5) are independent, atomic, single-file operations, so an interrupt between them leaves
each file in a fully valid, already-correct-for-what-was-answered state; re-running `am
setup` resumes cleanly because every step is idempotent by construction (the effective-value
defaulting means re-asking an already-answered question is a no-op).

**EOF on stdin** (e.g. `am setup </dev/null` without `--yes`): the first prompt receiving
`Ok(0)` from `read_line` treats it as an abort — one message ("no input received; re-run
with --yes for non-interactive setup"), exit 1, no further steps run. This is a backstop for
stdin being a TTY that unexpectedly closes mid-flow, not the primary non-interactive path
(see below).

## Non-interactive / non-TTY behaviour

- **`am setup --yes`**: never prompts. Every question above resolves to its effective
  current value — which, per the no-op rule, means **`am setup --yes` on an already-fully-
  configured repo writes nothing at all** and degrades to "run doctor verification and print
  the report." On a fresh repo it writes the skeleton files with the detected-credential/
  compiled defaults (no explicit overrides), exactly as UC1 with every prompt Enter'd
  through. Exit code **is doctor's exit code** (0 clean, 1 on any failure), so
  `am setup --yes && am start feat --agent claude` is a valid CI bootstrap step. Step 8
  (first session) never runs under `--yes` regardless of outcome — starting a session is a
  meaningfully mutating, possibly slow (image build) action a non-interactive bootstrap step
  should not take unasked.
- **`--agent <name>`** is not a prompt-default, it's a direct instruction: it's evaluated and
  potentially written **identically whether or not `--yes` is also passed**. If it names an
  agent different from the project file's current explicit value (or from the effective
  value if nothing is explicit yet), it's written; if it matches, nothing is written. This is
  why `am setup --yes --agent claude` is a meaningful, deterministic way to *change* an
  existing repo's agent from a script — it does not fall under the "yes means no-op" rule
  above, because a flag is not "accepting a default," it's supplying an answer.
- **`am setup` with stdin not a TTY and no `--yes`**: detected via `std::io::IsTerminal`
  (stable since Rust 1.70, well under `am`'s actual build floor — the crate graph today
  already requires 1.88 via transitive dependencies, `home` 0.5.12 in particular; this
  requires no MSRV concession). Fails fast with one message: `am setup requires an
  interactive terminal — pass --yes for non-interactive setup, or use 'am init' +
  'am generate-config'` — no files are touched. This protects against a CI script or a piped
  invocation silently hanging on a prompt that will never receive input.

## What it writes

**Rule: `defaults.agent` is a per-repo decision and is always written to `.am/config.toml`
(never to the global file, no matter where the shown default came from). `container.enabled`
is a host decision and is always written to `~/.config/am/config.toml`.** These are the only
two keys `am setup` ever writes — see [Resolved Decision #4](#resolved-decisions) for why the
scope stays this narrow.

There are two distinct write paths, because a file that doesn't exist yet and a file the user
may have hand-edited need different treatment:

**Creating a file that doesn't exist** uses a plain string template — `render_project_config_
skeleton()` / `render_global_config_skeleton()` — matching `config::write_defaults`'s existing
style: fully commented, documented, human-readable. There's no existing content to preserve,
so there's nothing format-preservation buys here.

**Updating a file that already exists** uses **`toml_edit`** (new dependency — see below),
which parses the file into an editable document, finds-or-inserts exactly the one key being
changed, and re-serializes — preserving every comment, blank line, table order, and
formatting choice the user (or a previous `am setup` run) made everywhere else. This is the
part of the [OQ5 override](#resolved-decisions) that matters: an update path built on line
matching or regex would be the first thing to break on a file someone has actually edited,
which is precisely the file `am setup` is now expected to handle correctly.

```rust
/// Returns Ok(true) if the file was written, Ok(false) if the requested value already
/// matched what's there (no-op — the file's mtime is not touched).
pub fn update_project_agent(path: &Path, agent: container::KnownAgent) -> Result<bool>;
pub fn update_global_container_enabled(path: &Path, enabled: bool) -> Result<bool>;
```

**Why hand-roll prompts but add a dependency for TOML editing — these are not
inconsistent.** The prompt flow is a handful of short, linear questions with no need for
cursor-based navigation; the strongest reason to avoid a prompt crate is that `dialoguer`-
style crates read the TTY directly, which fights the very `Io`/`ScriptedIo` seam that makes
the question logic unit-testable (see [Resolved Decision #2](#resolved-decisions)) — that
argument has nothing to do with dependency count and doesn't apply to `toml_edit` at all.
`toml_edit` isn't chosen to save code, it's chosen because *correctness of an in-place edit
to a file the user may have hand-edited* is a category of bug (silently mangling their
comments, dropping a table, reordering things) that hand-rolled line-matching cannot be made
to reliably avoid, and getting it wrong writes bad data into a file `am` itself asked to be
allowed to edit. Different problems, different tools, both reasoned about explicitly here so
the choice doesn't read as an inconsistency.

`toml_edit` 0.25.13's MSRV is 1.85, which matches the floor `am` already needs to build, so
this adds no new constraint. It also shares its dependency graph with `toml` (already a direct
dependency): with `toml` 1.0, `cargo tree -d` reports **no duplicates at all** across
`toml_datetime`, `toml_parser`, `toml_writer`, `winnow`, and `indexmap` — the two crates agree
on a major version of each, so the marginal addition to the crate graph is only `toml_edit`
itself.

(Under the `toml` 0.9 this spec was originally written against, `toml_datetime` and `winnow`
each resolved to two versions. The `toml` 1.0 upgrade on `main` removed that split.)

**No-op writes never touch the file.** `update_project_agent`/`update_global_container_
enabled` compare the requested value against what's already there (parsed, not
string-matched) before writing anything, and return `Ok(false)` — no `std::fs::write` call at
all — when they're equal. This is what makes "press Enter through everything" and
`am setup --yes` on an already-configured repo genuinely no-ops, not merely idempotent
rewrites that happen to produce the same bytes.

## Verification step

`am setup`'s verification step is not a new check catalogue — it is a direct call to
`doctor::run(Some((&repo_root, vcs)), agent_flag)` (the identical function `am doctor`'s
command handler calls), rendered with the existing `Report::render()`, run *after* steps 4-5
have made whatever changes they made. The only thing `am setup` adds on top is the framing
text ("Checking your setup...") before it and, on success, a "Next steps" block after it:

```
Ready.

Next steps:
  am start feat --agent claude   # start your first session
  am doctor                      # re-check readiness any time
  am attach feat                 # jump back into a running session
```

On failure, the report itself (with its `hint` lines) is the guidance; `am setup` adds
nothing beyond a pointer to re-run once the flagged items are fixed.

## API / contract surface

### CLI (`src/cli.rs`)

```rust
/// Guided, interactive setup — init plus a short question flow plus verification.
Setup {
    /// Skip all prompts; use effective current values for every question.
    #[arg(short, long)]
    yes: bool,
    /// Explicitly set (and, if it differs from what's there, write) the agent.
    #[arg(short, long)]
    agent: Option<String>,
},
```

### New module: `src/onboarding.rs`

Owns everything specific to the guided flow. Reuses `config`, `container`, `devcontainer`,
`doctor`, `tmux` for detection and verification; reuses `main::cmd_init`'s extracted helper
and `main::cmd_start` for the two mutating actions it delegates rather than reimplements.

```rust
/// Where an effective value currently comes from — needed to label prompts ("(from your
/// global config)") and, for the agent question specifically, to decide the prompt's
/// pre-filled default without ever deciding the WRITE target (that's a fixed rule, not
/// derived from source — see "What it writes").
pub enum Source {
    Project,
    Global,
    CompiledDefault,
}

pub struct Effective<T> {
    pub value: T,
    pub source: Source,
}

/// What `am setup` already knows without asking, gathered once up front so the question
/// flow can decide what to skip and what default to show. Mirrors the inputs `doctor::run`
/// and `cmd_start` use, so a question is never asked about something those functions could
/// answer themselves.
pub struct DetectedState {
    pub vcs: Option<config::Vcs>,             // None if not in a repo
    pub project_config_path: PathBuf,
    pub project_config_exists: bool,
    pub global_config_path: Option<PathBuf>,
    pub global_config_exists: bool,
    pub tmux_present: bool,
    pub runtimes_found: Vec<container::RuntimeKind>,   // 0, 1, or 2 entries
    pub devcontainer: Option<PathBuf>,
    pub agent_credentials: Vec<(container::KnownAgent, bool)>, // probed, never displays secrets
    pub effective_agent: Effective<Option<container::KnownAgent>>,
    pub effective_container_enabled: Effective<bool>,
}

impl DetectedState {
    pub fn gather(repo_root: Option<&Path>) -> Result<Self> { /* ... */ }
}

/// Resolved answers — `None` means "no write needed for this key."
pub struct Answers {
    pub agent: Option<container::KnownAgent>,
    pub container_enabled: Option<bool>,
    pub start_session: Option<String>,               // slug, if the user opted in
}

/// The IO seam. `TermIo` is the real terminal; `ScriptedIo` (test-only) replays a fixed
/// list of answers and captures output, so prompt logic (defaults, retry-on-invalid, EOF)
/// is unit-testable without a subprocess or a real TTY.
pub trait Io {
    fn prompt_line(&mut self, question: &str) -> Option<String>; // None = EOF
    fn println(&mut self, line: &str);
}

pub struct TermIo;              // std::io::IsTerminal-gated in the caller
#[cfg(test)]
pub struct ScriptedIo { /* Vec<String> answers, String captured output */ }

/// Steps 4-5 of the question flow, against any `Io`. Returns only the keys that actually
/// changed; steps 2-3 (ensure-file-exists) and 6-8 live in `cmd_setup` because they call
/// into `main`/`doctor` directly.
pub fn ask_agent(
    io: &mut dyn Io,
    detected: &DetectedState,
    agent_flag: Option<container::KnownAgent>,
) -> Option<container::KnownAgent>;

pub fn ask_container_enabled(
    io: &mut dyn Io,
    detected: &DetectedState,
) -> Option<bool>;

// Greenfield creation — no existing file, nothing to preserve.
pub fn render_project_config_skeleton() -> &'static str;
pub fn render_global_config_skeleton() -> &'static str;

// Existing-file update — format-preserving, via toml_edit. Ok(true) = wrote, Ok(false) =
// no-op (value already matched, file untouched).
pub fn update_project_agent(path: &Path, agent: container::KnownAgent) -> anyhow::Result<bool>;
pub fn update_global_container_enabled(path: &Path, enabled: bool) -> anyhow::Result<bool>;
```

### `main.rs`

```rust
fn cmd_setup(yes: bool, agent_flag: Option<&str>) -> anyhow::Result<()> {
    // 2-3: ensure project + global config files exist (shared helper with cmd_init for the
    //      project half; an equivalent trivial helper for the global skeleton).
    // 4-5: onboarding::DetectedState::gather, then ask_agent / ask_container_enabled
    //      (skipped/auto-answered under --yes; --agent always evaluated regardless of --yes);
    //      any Some(value) returned is applied via update_project_agent /
    //      update_global_container_enabled.
    // 7:   doctor::run(...) + report.render(), matching cmd_doctor's exit-code rule.
    // 8:   only when report.failures() == 0 && !yes && stdin is a TTY: prompt, then
    //      call cmd_start(&slug, agent_flag, false, false, false) directly — no
    //      duplicated worktree/tmux/container logic.
}
```

### `Cargo.toml`

```toml
[dependencies]
toml_edit = "0.25"
```

## Data model

No changes to `config::Config` or `session::Session` — `am setup` only ever produces the
same `.am/config.toml` / `~/.config/am/config.toml` shapes those types already parse, and
reads existing files through `toml_edit::DocumentMut` purely to preserve formatting on write,
not to replace `config::load_with_global`'s own parsing for actually running a session. The
new types (`DetectedState`, `Effective<T>`, `Source`, `Answers`, `Io`) live entirely in
`onboarding.rs` and don't persist anywhere; they exist only for the duration of one
`am setup` invocation.

## Testing strategy

Two layers, matching how the project already separates concerns:

- **Unit tests in `onboarding.rs`**, against `ScriptedIo` and temp files:
  - default-on-empty-input, re-prompt-on-invalid-input, EOF-aborts-cleanly
  - `render_project_config_skeleton`/`render_global_config_skeleton` produce fully commented
    output (nothing pre-activated)
  - `update_project_agent`/`update_global_container_enabled` on a file **with comments and
    non-default formatting** (hand-restructured tables, blank lines, trailing comments): the
    changed key's value updates and everything else round-trips byte-for-byte
  - calling either `update_*` function with a value equal to what's already there returns
    `Ok(false)` and leaves the file's mtime and bytes untouched
  - `DetectedState::effective_agent`/`effective_container_enabled` correctly label source as
    `Project`/`Global`/`CompiledDefault` across the three precedence cases
- **Cucumber integration tests** (`tests/features/setup.feature`), exercising only
  `am setup --yes[, --agent <name>]` — the project's existing subprocess harness has no seam
  for feeding interactive stdin, and this combination happens to cover both the no-op path
  and the deterministic-change path without needing one:
  - fresh repo, nothing configured → both config files written, doctor section (mocked
    runtime/tmux) reports ready, exit 0
  - fresh repo with an existing global config setting `agent = "claude"` → project file is
    written but its `agent` line stays commented (inherited), matching UC2
  - `am setup --yes` on an already-fully-configured, doctor-clean repo → neither config
    file's mtime changes (asserted via the step definitions), exit 0
  - `am setup --yes --agent claude` on a project file that already has `agent = "codex"` →
    the file is rewritten to `claude`, and a snapshot assertion confirms surrounding comments
    are unchanged
  - `am setup --yes --agent codex` on a project file that already has `agent = "codex"` → no
    rewrite (mtime unchanged) — exercises the no-op comparison end-to-end, not just at the
    unit level
  - no container runtime mocked in → doctor section fails, exit code is 1, "Next steps" is
    NOT printed (verifies step 8 is unreachable on failure)
  - `--agent unknown-agent` → fails immediately with the existing unknown-agent error, before
    any file is written
  - non-TTY without `--yes` → the cucumber harness already runs subprocesses with piped
    stdin, so this is the default condition for every scenario above too; a dedicated
    scenario asserts the specific "requires an interactive terminal" message when `--yes` is
    omitted.

## Task breakdown

1. **backend-engineer** — `src/cli.rs`: add `Commands::Setup { yes, agent }` with clap tests
   matching the existing `Start`/`Session` patterns.
2. **backend-engineer** — refactor: extract the body of `cmd_init` (directory + gitignore
   handling) into a helper function callable from both `cmd_init` and `cmd_setup`, with
   existing `init.feature` scenarios re-run unchanged to confirm no behavior shift.
3. **backend-engineer** — `Cargo.toml`: add `toml_edit = "0.25"`; confirm `cargo build` and
   `cargo tree` show no MSRV or duplicate-TOML-stack surprise beyond what's noted above.
4. **backend-engineer** — `src/onboarding.rs`: `Effective<T>`/`Source`, `DetectedState::
   gather` (including the project/global/compiled-default precedence resolution for both
   tracked keys), `Io`/`TermIo`/`ScriptedIo`, `ask_agent`, `ask_container_enabled`,
   `render_project_config_skeleton`, `render_global_config_skeleton`,
   `update_project_agent`, `update_global_container_enabled` (`toml_edit`-based), plus the
   unit tests described above.
5. **backend-engineer** — `main.rs::cmd_setup`: wire detection → prompts → the two `update_*`
   calls → `doctor::run` → optional `cmd_start` call; TTY detection via
   `std::io::IsTerminal`; exit code matches doctor's.
6. **integration-tester** — `tests/features/setup.feature` per the scenarios above,
   including a mtime-unchanged assertion helper if the existing step definitions don't
   already have one.
7. **code-reviewer** — standard review pass once 1-6 are green; particular attention to:
   the no-secrets-written rule (credentials are only ever probed for presence, never
   displayed or written into a config file); the exit-code contract for `--yes`; and
   `toml_edit` correctness on documents with tables in non-default order or with unrelated
   custom keys — this is exactly the class of file the update path exists to handle safely.
8. **documentation-writer** — `docs/reference/commands.md`: new `## am setup` section
   matching the existing per-command format (Usage / What it does / example output);
   `README.md` quick-start: mention `am setup` as the guided alternative to `am init` for
   first-time users; `BACKLOG.md`: add a tracked entry linking this spec.

## Edge cases & considerations

- **Security — no secret ever transits a prompt or a config file.** `am setup` only ever
  probes for the *presence* of credentials (file exists / env var set), the same booleans
  `validate_agent_credentials` already produces for `am doctor`. It never asks a user to
  paste a token and never writes one into `.am/config.toml` (which is meant to be committed)
  or `~/.config/am/config.toml`. If `OPENAI_API_KEY` isn't set for `codex`, the output is a
  hint to `export` it, identical in spirit to what `am doctor` already prints.
- **Race conditions:** none introduced. Greenfield writes are a single complete
  `std::fs::write`; existing-file updates via `toml_edit` read-modify-write the whole file
  in one pass, same single-writer assumption every other `am` config write already makes (no
  concurrent-writer protection exists anywhere in the codebase today, and `am setup` doesn't
  need to be the first place that adds it).
- **Cosmetic wrinkle in the update path, narrowed during implementation:** the greenfield case
  (no project file yet, and `am setup` already knows the agent from `--agent` or `--yes`'s
  default) no longer produces a duplicate — `render_project_config_skeleton_with_agent`
  renders the `defaults.agent` line active from creation instead of inserting it next to its
  own commented example (`am init`'s own output is unchanged: still fully commented). The
  wrinkle can still occur on the update path proper — `update_project_agent` inserting into a
  project file that predates this run and still carries the commented example — and that case
  is accepted for the same reason as before: the alternative is detecting and rewriting the
  specific commented example line, which is exactly the class of fragile line-matching adopting
  `toml_edit` was meant to avoid. It doesn't affect correctness (TOML comments have no semantic
  weight), only tidiness.
- **Devcontainer `initializeCommand` gate:** explicitly never auto-enabled by the wizard
  (see step 6) — this is the one place a guided default would be actively unsafe, so it's a
  hard no rather than a "maybe ask" question.
- **Stale `container.enabled = false` after a runtime becomes available:** if a previous
  `am setup` run disabled containers (no runtime was found at the time) and a runtime is
  later installed, `am setup` does not proactively re-offer re-enabling it on a subsequent
  run, because the containers question's trigger ("no runtime found") no longer holds. This
  is an explicit, narrow scope boundary, not an oversight: `container.enabled = false` is a
  valid deliberate state, `am doctor` reports it as a warning (not a failure) either way, and
  the user can flip it by hand or via `am setup --yes` is not the mechanism for that (it only
  writes what a question resolves to, and this question won't be asked). Worth a one-line
  mention in the "Next steps" summary is out of scope for v1.
- **First-session step and image builds:** if the repo resolves to devcontainer mode and no
  image is built yet, accepting step 8 can take minutes (the same build `am start` would
  trigger). The prompt text should say so ("this may take a few minutes to build the
  environment") rather than let the wizard appear to hang.
- **Idempotency:** running `am setup` twice in a row is safe and, past the first run,
  effectively silent when nothing has changed on the host or in the files — it degrades to
  "verify and report" precisely because every write is now genuinely a no-op when the
  effective value is unchanged, not because the second run takes a different code path.

## Resolved Decisions

All seven items originally raised as Open Questions have been decided with the user. Recorded
here so they aren't re-litigated during implementation.

1. **Command name: `am setup`.** Accepted as recommended.
2. **Prompt crate vs. hand-rolled: hand-rolled**, behind the `Io` trait, extending the
   existing `print!`/`read_line` pattern (`am destroy`/`am session rm`). Accepted, but the
   original justification leaned on the project's Rust-version floor, which turned out to be
   1.88 (via `home`/`clap_builder`/`sha2`/`toml_parser`), not the 1.70 the docs claim
   (`docs/reference/building.md` is stale — tracked separately in BACKLOG.md, not part of
   this feature). Both `dialoguer` and `inquire` would have been fine on MSRV grounds, so
   that reasoning is dropped. The decision stands on two arguments that don't depend on it:
   (a) `dialoguer`-style crates read the TTY directly, which fights the `Io`/`ScriptedIo`
   seam the unit tests above depend on — this is the load-bearing reason; (b) dependency
   surface (8-36 additional crates) against a release profile tuned hard for size
   (`opt-level = "z"`, LTO, strip).
3. **Relationship to `am init`: separate command.** Accepted as recommended — `am setup`
   calls `init`'s extracted logic as step one; `am init` itself is unchanged.
4. **Global config scope: narrow — `defaults.agent` and `container.enabled` only.**
   Accepted, with one precision fix along the way: the original write-up called the second
   item "container runtime," which could be misread as picking between podman and docker.
   `RuntimePreference::Auto` already resolves that correctly without asking (podman first,
   then docker); the actual question is whether a usable runtime exists *at all*, and the
   key it resolves is `container.enabled`, not `container.runtime`. This is a wording/
   precision correction, not a scope change — the underlying "only when auto-detection is
   ambiguous" trigger is exactly what was agreed.
5. **Existing-config behaviour: show current values, offer to change them — OVERRIDE of the
   original "never touch existing files" recommendation.** This is the substantial revision
   in this pass: the agent question is now always asked (not skipped once a project config
   exists), its default is the effective current value with its source shown, accepting the
   default writes nothing, and a real change is written via a format-preserving `toml_edit`
   update rather than a from-scratch overwrite. UC2 and UC3 both changed shape accordingly
   (see above). No objection to raise here — the override addresses a real gap in the
   original design (UC2, "adding `am` to a new repo," is exactly the case where "never touch
   an existing file" made `am setup` a near-alias for `am doctor`). The one tradeoff worth
   flagging is already called out under Edge Cases (a cosmetic comment-duplication wrinkle
   on first edit) — it's a known, accepted cost of the format-preserving approach, not a
   reason to reconsider it.
6. **Declining the first-session prompt still exits 0.** Accepted as recommended.
7. **`--yes` does not start a session unattended.** Accepted as recommended — `am setup
   --yes` remains a pure bootstrap-and-verify (and, with `--agent`, deterministic-change)
   step, never one that also creates a worktree/branch/container unasked.

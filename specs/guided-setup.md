# Feature: Guided Setup (`am setup`)

A new, interactive command that walks a first-time or new-repo user through configuring
`am`, asking only the questions that detected state can't answer on its own, then verifying
the result and optionally launching a first session.

**Status:** the agent/containers flow described here shipped in PR #47. This revision adds a
third question — pane layout — because the shipped flow, on a machine that already has a
container runtime installed, asked exactly one question (agent), which undershot the user's
original "make this as easy as possible" goal by collapsing hand-holding to almost nothing
in the common case. A follow-up resolution then extended all three questions to state where
their answer will be saved, not just the new one — see
[Resolved Decisions](#resolved-decisions) #9. This document also brings the API/data-model
sections in line with what actually shipped, since both additions sit directly alongside
them.

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
  to earn its place — but "earning its place" cannot mean "collapses to zero questions on a
  well-equipped machine," which is the specific failure the pane-layout addition corrects. A
  question about a genuine, undetectable user preference earns its place by existing at all,
  not by being gated behind a rare failure condition.
- Users already comfortable with `am` keep using `am init` for new repos, `am doctor` to
  debug, and hand-editing `.am/config.toml` for anything outside the handful of settings
  `am setup` knows how to ask about. `am setup` doesn't replace those workflows or grow into
  a general config editor — see [Resolved Decisions](#resolved-decisions) #4.
- No new prompt-UI dependency is worth adding for this; a format-preserving TOML-edit
  dependency is (see #2 and #5, and [What it writes](#what-it-writes)).

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
3. The agent question is asked — stating up front that it saves to *this repo's* config
   (`.am/config.toml`) — with a default derived from the *effective* current value; here
   nothing is configured anywhere, so the default is the first agent already authenticated
   on this host, else `claude` (see [Question flow](#question-flow)). Answering writes it
   into the project file.
4. The containers question — stating up front that it saves *machine-wide*
   (`~/.config/am/config.toml`) — is asked only if no runtime is currently found on the
   host; if answered, it's written into the global file.
5. The pane layout question — same machine-wide framing — is always asked (this is the
   point of the layout revision — it is not gated behind any detection condition beyond
   "there is a global file to write it to"). A chosen preset or a customized combination is
   written into the global file.
6. `am setup` runs the same checks `am doctor` runs and prints the report inline.
7. If the report is clean, `am setup` offers to start a first session.
8. User accepts, provides a slug, `am setup` calls the same code path as `am start`.

**Postcondition:** `.am/config.toml` and `~/.config/am/config.toml` exist with the user's
choices active; a running session may exist if the user opted in.

### UC2 — Existing user, adding `am` to a new repo

**Actor:** a user with a working global config from a previous repo, including a pane layout
they already chose once.
**Preconditions:** inside a repo with no `.am/config.toml`. `~/.config/am/config.toml`
already exists, sets an agent, and sets a non-default pane layout.
**Main flow:**

1. User runs `am setup`.
2. A fresh project config skeleton is created; the existing global config is opened, not
   recreated.
3. The agent question states it saves to *this repo's* file, and shows its default sourced
   from the **global** config ("currently: claude (from your global config)") — the prompt
   makes both facts legible together: where a change would land (this repo only) and where
   the value shown is actually coming from right now (global). Pressing Enter accepts the
   default, and because the answer didn't change, nothing is written to the project file,
   which keeps its `agent` line commented and continues to inherit from global.
4. The containers question is asked only if the trigger condition currently holds (no
   runtime found); if a previous run already resolved this, accepting its default writes
   nothing.
5. The layout question is still asked (it is not gated by "global config already sets one" —
   see [Question flow](#question-flow)), but its default is now the layout already saved
   globally, labeled `(from your global config)`. Pressing Enter through the preset menu (or
   picking the preset that happens to match) writes nothing.
6. Verification runs and prints; offer to start a first session, as UC1.

Once a user has a healthy global config, this now involves at most three real answers —
agent, containers, layout — each defaulting correctly, so a returning user who wants nothing
to change can press Enter three times and land on the same "verify and report" outcome the
previous revision reached with fewer prompts. The tradeoff is deliberate: see
[Resolved Decisions](#resolved-decisions) #8.

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
   choice, not just re-report it. The prompt's write-target line already told them this
   lands in `.am/config.toml` — the file they're here to fix.
4. If no runtime is currently found, the containers question offers to disable containers so
   the user can keep working without one, or the user can leave it as-is and go install one.
5. The layout question runs too, unrelated to whatever brought the user here — it's asked on
   every run, not conditioned on there being a problem to fix.
6. `doctor::run()` re-verifies against whatever is now in the (possibly just-updated) project
   config and prints the report — reflecting any change just made in step 3-4 (layout has no
   bearing on doctor's checks).
7. Clean → offer a first session. Not clean → the report's own hints are the guidance;
   `am setup` stops there.

**Scope boundary, restated:** `am setup` can change the things it already knows how to ask
about — which agent, whether containers are enabled, and pane layout — on an existing
config. It still does **not** become a general repair tool: it never installs a container
runtime, never runs `gh auth login` or writes a token, and never edits any config key outside
`defaults.agent`, `container.enabled`, and the three `tmux.*` layout keys.

## Scope boundary vs. `am init` and `am doctor`

**A new, separate command — `am setup` — not a flag on `init`.** `am init` must stay fast,
silent, and scriptable: it's invoked from the cucumber fixtures and (per the docs) from setup
scripts as an idempotent, non-interactive primitive, and making it interactive by default
inverts that contract for every existing caller. This also matches the "fast path vs. custom
path" split BACKLOG.md already calls for: `am init` (fast, scriptable) and `am setup`
(guided, interactive) are the two doors.

**`am setup` is additive, not a rewrite:** its first action is running the exact same logic
`am init` runs (`.am/` + `.gitignore`), factored into a shared helper (`init_project`, in
`main.rs`) so the two commands cannot drift apart — the same "don't let a passing check and a
working command disagree" principle `am doctor` already follows for `cmd_start`'s preflight
functions.

**Relationship to `am doctor`: `am setup`'s verification step *is* `doctor::run()`.** No new
check logic is written. `am setup` computes what's already configured using the same
primitives `doctor::run()` and `cmd_start` already call (`container::detect_runtime`,
`container::validate_agent_credentials`, `devcontainer::find_config`, `tmux::find_tmux`), and
after writing config it calls `doctor::run()` for real and renders it with the existing
`Report::render()` — byte-for-byte the same output `am doctor` would print for that repo.
Pane layout is not a doctor check (a "wrong" layout doesn't stop `am start` from working), so
it plays no part in the pass/fail verdict — it's purely a preference question layered
alongside the two questions that do matter to readiness.

This does **not** reverse the stance BACKLOG.md records for `am doctor` ("the alternative to
auto-bootstrapping `.am/` as a side effect of `am start`"). `am start` still does not
auto-bootstrap anything. `am setup` is an explicit, user-invoked command whose entire job
*is* bootstrapping — it's the opt-in front door, not a side effect.

## Question flow

The guiding rule remains **ask only what detected state can't answer** — but the layout
revision draws a sharper line for what counts as "answerable by detection." Agent credentials
and container runtimes are things a probe can genuinely determine. Pane layout is not: no
amount of host inspection tells `am` whether a user wants the agent on the left, on the
right, or stacked on top. A question in the second category doesn't lose its justification
just because it's always asked.

A second, orthogonal rule now applies uniformly to all three questions (the follow-up
resolution — see [Resolved Decisions](#resolved-decisions) #9): **every question states
where its answer will be saved**, in terms of scope first ("just this repo" vs. "every repo
on this machine") and the file path second. This is a distinct concern from the "currently:"
line each question already showed — that line says where the *displayed default* came from;
the new line says where a *change* would go. For the agent question specifically these two
facts can point at different files at once (default from global, write to project), which is
exactly why both need to be legible in the same prompt, not just one or the other.

```
1. Preconditions (no prompt)
   ├─ not in a repo?              → same error as `am init`, exit
   └─ in a repo                   → proceed, VCS (git/jj) detected silently

2. Project config file (no prompt — an action, not a question)
   ├─ .am/config.toml missing     → write skeleton + .gitignore entry (= `am init`)
   └─ .am/config.toml exists      → open it, don't touch it yet — its values feed step 4

3. Global config file (no prompt — an action, not a question)
   ├─ ~/.config/am/config.toml missing  → write skeleton
   └─ ~/.config/am/config.toml exists   → open it, don't touch it yet — feeds steps 4-6

4. Agent question — ALWAYS asked, unless --agent or --yes was passed
   States: saves to THIS REPO's config. Default = effective value via project → global →
   compiled default, labeled with source. Accepting it writes nothing. A change writes
   `defaults.agent` into the PROJECT file only, regardless of which file the shown default
   came from.

5. Containers question — asked ONLY if neither podman nor docker is currently on PATH (and
   there is a global file to write to — see below)
   States: saves MACHINE-WIDE. Default = effective `container.enabled`, read from the GLOBAL
   file only (a project-level override of this key, if one exists, is intentionally out of
   scope for this question). Accepting it writes nothing. A change writes `container.enabled`
   into the GLOBAL file.

6. Pane layout question — ALWAYS asked, unless --yes was passed or there is no global file
   to write to (`detected.global_config_path` is `None`) — the same "nowhere to save the
   answer" gate `ask_container_enabled` already uses, and the only gate this question has.
   States: saves MACHINE-WIDE, same as containers. See the dedicated section below.

7. Project-specific notes (no prompt — informational only)
   ├─ .devcontainer/devcontainer.json found → one line: "found — sessions will use it
   │    automatically (container.mode = auto)". No question: auto is already the correct
   │    default, and asking "use your devcontainer?" when the answer is obviously yes fails
   │    the "every question must justify itself" bar.
   │    If it also contains `initializeCommand` (which `am` refuses by default): a warning
   │    line pointing at `am doctor` and `devcontainer.allow_host_commands`, NOT a prompt —
   │    silently enabling host command execution from a wizard default is not acceptable.
   └─ none found                 → nothing printed; image mode is already the default

8. Verification (no prompt)
   → run doctor::run() against the resolved repo + agent (reflecting whatever was just
     written in steps 4-6), render exactly as `am doctor`
   → 0 failures  → continue to step 9
   → failures>0  → print the report, exit 1 (see UC3); step 9 is not reached

9. First session (prompt only if step 8 passed AND session is interactive, i.e. not --yes
   and stdin is a TTY)
   → "Start your first session now? [Y/n]"
        no / declined  → print next-step commands, exit 0
        yes            → "Session name: " (two tries, no default; falls through to
                           declining rather than looping forever on a required field)
                        → calls the same function `cmd_start` uses, with the resolved agent
```

**Why the layout question sits between containers and the informational notes, not
elsewhere:** it groups with the other two prompts (agent, containers) as the third and last
thing the user is actually asked, keeping every interactive Q&A together before the flow
moves into report-only territory (the devcontainer note, then verification). Putting it after
verification would suggest it's diagnostic, which it isn't; putting it before the agent
question would ask about *how a session looks* before establishing *what runs in it*, the
wrong order for a first-time user building a mental model of the tool.

**Empty input, invalid input, Ctrl-C, EOF:** unchanged — accept the shown default on empty
input; re-ask with a one-line reason on invalid input, no retry limit; Ctrl-C is default
process behavior with no rollback needed (every write is independently idempotent); EOF
aborts with one message and a non-zero exit. The layout question's sub-flow (see below)
follows the same idiom at each of its own prompts.

### The write-target line, shared by all three questions

One line, printed before each question's body, naming the scope first and the path second —
scope is what a user actually decides between ("do I want this everywhere, or just here?"),
the path is the detail that makes it verifiable:

```rust
/// Where a question's answer is saved — fixed per question, independent of where the
/// *displayed default* happens to be read from (that's `Source`, a different question:
/// "where did this value come from" vs. "where would a change go").
enum WriteScope {
    Project,
    Global,
}

impl WriteScope {
    fn phrase(self) -> &'static str {
        match self {
            WriteScope::Project => "just this repo",
            WriteScope::Global => "every repo on this machine",
        }
    }
}

/// One line, shared by `ask_agent`, `ask_container_enabled`, and `ask_layout` — a single
/// implementation so the wording cannot drift between them, and so it's pinned by one set
/// of tests instead of three copies that could disagree.
///
/// `base` is what `path` gets shortened against for display — `detected.repo_root` for
/// `WriteScope::Project`, `detected.home_dir` for `WriteScope::Global` — so a 80+ character
/// absolute project path doesn't wrap and defeat the point of an at-a-glance line. `None`
/// (no known base, or `path` isn't actually under it) falls back to the absolute path.
fn write_target_line(label: &str, scope: WriteScope, path: &Path, base: Option<&Path>) -> String {
    let shown = shorten_for_display(scope, path, base);
    format!("{label} — {}; saved to {}.", scope.phrase(), shown.display())
}
```

Concrete output for each question:

```
Agent — just this repo; saved to .am/config.toml.
```
```
Containers — every repo on this machine; saved to ~/.config/am/config.toml.
```
```
Pane layout — every repo on this machine; saved to ~/.config/am/config.toml.
```

Each is printed as the first line of the question, before the menu/prompt body, and before
the existing "currently: ..." line. For the agent question in particular, the two lines
together answer both of the user's questions in the order they'd ask them: "where would my
answer go?" (this line) then "where's the current default coming from?" (the existing
`Source`-labeled line, which may name a *different* file — see UC2).

**This changes already-shipped output.** `ask_agent` and `ask_container_enabled` did not
print this line before this revision; adding it is an intentional behavior change, not a
regression, so existing assertions that pinned their exact prompt text move with it — called
out explicitly in [Testing strategy](#testing-strategy) and
[Task breakdown](#task-breakdown) so it isn't mistaken for a broken test during review.

### The pane layout question, in detail

**Presets, not three granular questions up front.** A single question offers four common
layouts plus "customize…", each shown with a small preview so the user doesn't have to
mentally simulate what "agent right, 70/30" looks like:

| # | Preset | `agent_pane` | `split` | `split_percent` |
|---|---|---|---|---|
| 1 | agent left, 50/50 | `left` | `horizontal` | 50 |
| 2 | agent right, 50/50 | `right` | `horizontal` | 50 |
| 3 | agent left, 70/30 | `left` | `horizontal` | 70 |
| 4 | stacked, agent on top, 50/50 | `left` | `vertical` | 50 |
| 5 | customize… | — falls through to three granular sub-questions | | |

Preset 1 is `am`'s compiled default (`TmuxConfig::default()`), which is why it's what a
first-time user with nothing configured sees pre-selected.

**Why `left` for "stacked, agent on top" (#4), and why it isn't ambiguous:** `PaneSide::Left`
already means "the agent's pane is the one placed *before* the other" — `tmux.rs`'s
`split_window` takes a `before: bool` that "places the new pane left of (horizontal) or
above (vertical) the existing one," and `pane_layout(&PaneSide::Left)` passes `before: true`
and puts the agent in the resulting first pane. So `left` already means "top" the moment
`split` is `vertical`; the schema doesn't grow a fourth value for stacked layouts, it reuses
the two it has. **This is exactly the wrinkle the customize path must not paper over**: for a
horizontal split, the natural words are "left"/"right"; for a vertical split, the same
`PaneSide` values read as "top"/"bottom", and asking "left or right?" once the user has
already chosen a stacked split would be simply wrong. Only one stacked preset is offered
(agent on top) — the inverse (agent on bottom) is reachable through customize, which is
enough given the goal is fewer decisions, not a preset for every combination.

**Preview rendering — one shared function, computed, not authored per preset.** The four
preset previews and the one customize preview all go through the same `render_layout`, rather
than being hand-written text for the fixed set — one rendering implementation to keep correct,
not four-plus-one. Each preset's preview prints on its own indented line(s) below the preset's
label line, not inline with it. Illustrative shape (exact spacing is an implementation detail,
not load-bearing):

```
Pane layout — every repo on this machine; saved to ~/.config/am/config.toml.
Which layout do you want?
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

Enter accepts the currently effective triple, not a hardcoded preset — the same "Enter means keep what's already in effect" idiom `ask_agent`'s and `ask_container_enabled`'s own defaults follow. On a first-time run with nothing configured, the effective triple happens to equal preset 1, but the prompt's wording does not assume that.

The opening line is `write_target_line`'s output, described above — the same helper the
agent and containers questions now use, not a one-off sentence specific to layout.

**Customize sub-flow — direction first, then a direction-aware pane question, then
percent.** This ordering is the only one that produces correctly worded questions: the pane
question's wording (left/right vs. top/bottom) *depends on* the direction, so direction
cannot be asked second. Concretely:

```
6a. "Side by side, or stacked?
       [1] side by side (horizontal)   [2] stacked (vertical)"
     Default = current effective split, with source. → chosen direction

6b. Horizontal chosen: "Which side should the agent be on? [1] left  [2] right"
    Vertical chosen:   "Should the agent be on top or on the bottom? [1] top  [2] bottom"
    Default = current effective agent_pane, worded to match the chosen direction.
    → chosen side ("top"/"left" both map to PaneSide::Left; "bottom"/"right" to
      PaneSide::Right — the prompt's words change, the stored value's meaning does not)

6c. "What percentage of the window should the agent pane get? [1-99] (Enter for 50):"
    Default = current effective split_percent, with source. Out-of-range or non-numeric
    input re-asks, same as every other invalid-input case in this flow.

6d. Render the resulting layout with the same preview format as the preset menu, then:
    "Use this layout? [Y/n]"
      accepted → the (side, direction, percent) triple is the answer
      declined → back to the top of the layout question (the preset menu), not a partial
                 retry of 6a-6c — simpler to reason about, and customize is rare enough that
                 re-entering it is not a real cost
```

The write-target line is shown once, at the top of the outer question (before "Which layout
do you want?"); the sub-questions don't repeat it — they're all still answering the one
question the header already scoped.

**Write target and per-key granularity.** All three keys are a per-user preference and are
always written to the **global** file, regardless of where the current effective value came
from — including the case where a project happens to already override one of them (see the
caveat below). Unlike the single-key agent/containers writes, a layout answer can touch up to
three keys at once, and a customize answer that only changes the percentage should not also
uncomment `agent_pane`/`split` lines that already say the right thing. `update_key` (see
[What it writes](#what-it-writes)) is called once per key, independently, so only the keys
that actually changed are touched.

**Caveat: a project-level `tmux.*` override.** The schema allows a project config to set its
own `[tmux]` values, which would outrank the global file for that repo specifically. If any
of the three effective values' source is `Project` when the layout question is about to be
asked, `am setup` prints one line before the prompt: "Note: this project's config already
sets its own pane layout — your answer here is saved globally and won't change sessions in
this repo until that override is removed." This is a judgment call, not something the user
was asked about directly, flagged as such in
[Resolved Decisions](#resolved-decisions) #8.5 — reasonable to ship as-is, cheap to adjust
later if it reads as noise in practice.

## Non-interactive / non-TTY behaviour

- **`am setup --yes`**: never prompts, so none of the write-target lines above are ever
  printed under `--yes` either — they're part of the interactive prompt bodies, not the
  summary output. The agent question still resolves to a proactive best-guess default when
  nothing is configured (the first agent already authenticated on this host, else `claude`)
  — because "no agent configured" is a real functional gap `am doctor` and `--auto` both
  care about. The containers question and the layout question are both **skipped entirely
  under `--yes`**, writing nothing, because neither has an analogous "unanswered means
  broken" stake: an unset `container.enabled` and an unset `tmux.*` both fall back to a
  working compiled default with no functional consequence. This is a deliberate asymmetry,
  not an oversight — see [Resolved Decisions](#resolved-decisions) #8.4. Net effect:
  `am setup --yes` on an already-fully-configured repo writes nothing at all and degrades to
  "run doctor verification and print the report"; on a fresh repo it writes only what's
  needed for a session to actually start. Exit code **is doctor's exit code**, so
  `am setup --yes && am start feat --agent claude` is a valid CI bootstrap step. Step 9
  (first session) never runs under `--yes` regardless of outcome.
- **`--agent <name>`** is not a prompt-default, it's a direct instruction, evaluated
  identically whether or not `--yes` is also passed, and it also skips the prompt (and
  therefore the write-target line) entirely — there's nothing to show a write-target line
  for when nothing was asked. There is no equivalent flag for layout or containers — see
  [Resolved Decisions](#resolved-decisions) #8.6 for why that's deliberately out of scope.
- **`am setup` with stdin not a TTY and no `--yes`**: detected via `std::io::IsTerminal`
  (`std::io::stdin().is_terminal()`, already the exact check in the shipped `cmd_setup`).
  Fails fast with one message before any file is touched.

## What it writes

**Rule: `defaults.agent` always goes to `.am/config.toml`; `container.enabled` and the three
`tmux.*` layout keys always go to `~/.config/am/config.toml`.** These are the only five keys
`am setup` ever writes — see [Resolved Decisions](#resolved-decisions) #4 for why the scope
stays this narrow, and #8 for why layout joined the list. Every one of the three questions
now states this rule to the user in its own prompt (see above), so it's no longer only
documented here — it's legible at the point of decision.

Two distinct write paths, unchanged in kind by this revision:

**Creating a file that doesn't exist** uses a plain string template
(`config::render_project_config_skeleton` / `onboarding::render_global_config_skeleton`),
fully commented, matching `config::write_defaults`'s existing style — there's no existing
content to preserve, so format preservation buys nothing here. One special case:
`onboarding::render_project_config_skeleton_with_agent` renders the skeleton with
`defaults.agent` already active, used only when `am setup` already knows the agent to write
(a flag, or `--yes`'s resolved default) *and* the project file doesn't exist yet — this
avoids inserting a real value next to its own commented-out example on a brand-new file.

**Updating a file that already exists** uses `toml_edit` (`onboarding::update_key`), which
parses the file into an editable document, finds-or-inserts exactly the key being changed,
and re-serializes — preserving every comment, blank line, table order, and formatting choice
already there. It refuses (rather than clobbers) a key that already holds a table, an inline
table, or an array — a hand-edited `[defaults.agent]` sub-table, say — since `am setup` only
ever writes a plain string or bool and silently discarding something structural is worse
than stopping and asking the user to fix it by hand.

```rust
// One key, one call each — already shipped.
pub fn update_project_agent(path: &Path, agent: container::KnownAgent) -> Result<bool>;
pub fn update_global_container_enabled(path: &Path, enabled: bool) -> Result<bool>;

// Three keys, one call per key internally — new in the layout revision. Returns the names
// of the keys actually written (a subset of ["agent_pane", "split", "split_percent"]), empty
// when the chosen layout already matched what was there.
pub fn update_global_tmux_layout(
    path: &Path,
    agent_pane: config::PaneSide,
    split: config::SplitDirection,
    split_percent: u8,
) -> Result<Vec<&'static str>>;
```

Every write compares the requested value against what's already there (parsed, not
string-matched) before touching the file, and skips the `std::fs::write` entirely when
they're equal — this is what makes "press Enter through everything" and `--yes` on an
already-configured repo genuinely no-ops, not merely idempotent rewrites that happen to
produce the same bytes.

**Why hand-roll prompts but use a dependency for TOML editing.** Different problems: the
prompt flow needs no cursor-based navigation, and the strongest reason to avoid a prompt
crate is that `dialoguer`-style crates read the TTY directly, fighting the `Io`/`ScriptedIo`
seam the unit tests depend on. `toml_edit` isn't chosen to save code — it's chosen because
correctness of an in-place edit to a file the user may have hand-edited is a category of bug
(mangled comments, a dropped table, silent reordering) hand-rolled line matching cannot
reliably avoid, and it's exactly the file `am setup` was given permission to edit. `toml_edit`
0.25's MSRV (1.85) is below `am`'s actual floor (1.88, via `home`/`clap_builder`/`sha2`), so
this adds no new constraint, and it shares much of its dependency graph
(`toml_datetime`, `toml_parser`, `toml_writer`, `winnow`, `indexmap`) with the `toml` 0.9
dependency `am` already has.

## Verification step

Unchanged: `am setup`'s verification is a direct call to
`doctor::run(Some((&repo_root, vcs)), agent_flag)`, run after every write (steps 4-6) has
happened, rendered with the existing `Report::render()`. On success it's followed by a
"Next steps" block (`print_next_steps` in `main.rs`); on failure the report's own hints are
the guidance and `am setup` exits 1 without reaching step 9.

## API / contract surface

This section matches the shipped module (`src/onboarding.rs`, `main.rs::cmd_setup`) for
everything except the layout addition and the write-target line, both new.

### CLI (`src/cli.rs`) — unchanged

```rust
Setup {
    #[arg(short, long)]
    yes: bool,
    #[arg(short, long)]
    agent: Option<String>,
},
```

No new flag for layout — see [Resolved Decisions](#resolved-decisions) #8.6.

### `src/onboarding.rs`

Already shipped, agent/container question logic unchanged except that `ask_agent` and
`ask_container_enabled` now each print `write_target_line(...)` as their first line:

```rust
pub enum Source { Project, Global, CompiledDefault }
pub struct Effective<T> { pub value: T, pub source: Source }

pub struct DetectedState {
    pub vcs: Option<config::Vcs>,
    // New in the layout revision, alongside `write_target_line`: the bases it shortens
    // `project_config_path`/`global_config_path` against for display. `repo_root` is `None`
    // alongside `vcs` being `None`; `home_dir` is `None` only when `HOME` isn't set.
    pub repo_root: Option<PathBuf>,
    pub project_config_path: PathBuf,
    pub project_config_exists: bool,
    pub global_config_path: Option<PathBuf>,
    pub global_config_exists: bool,
    pub home_dir: Option<PathBuf>,
    pub tmux_present: bool,
    pub runtimes_found: Vec<container::RuntimeKind>,
    pub devcontainer: Option<PathBuf>,
    pub agent_credentials: Vec<(container::KnownAgent, bool)>,
    pub effective_agent: Effective<Option<container::KnownAgent>>,
    pub effective_container_enabled: Effective<bool>,
    // New in the layout revision:
    pub effective_tmux_agent_pane: Effective<config::PaneSide>,
    pub effective_tmux_split: Effective<config::SplitDirection>,
    pub effective_tmux_split_percent: Effective<u8>,
}

pub trait Io {
    fn prompt_line(&mut self, question: &str) -> Option<String>;
    fn println(&mut self, line: &str);
}
pub struct TermIo;
```

`DetectedState::gather` extends its internal `TrackedKeys`/`resolve_effective` machinery
(the project-vs-global-vs-compiled-default reader already used for `defaults.agent` and
`container.enabled`) with a third tracked group:

```rust
#[derive(Debug, Default, serde::Deserialize)]
struct TrackedTmux {
    agent_pane: Option<config::PaneSide>,
    split: Option<config::SplitDirection>,
    split_percent: Option<u8>,
}
```

added to `TrackedKeys` alongside `defaults`/`container`, deserialized independently for the
same reason those two already are: a malformed `[tmux]` table should not mask a well-formed
`[container]` table in the same file, and vice versa.

New functions, this document's two additions combined:

```rust
// Write-target line — shared by all three questions.
enum WriteScope { Project, Global }
impl WriteScope {
    fn phrase(self) -> &'static str { /* "just this repo" | "every repo on this machine" */ }
}
fn write_target_line(label: &str, scope: WriteScope, path: &Path, base: Option<&Path>) -> String;

// Pane layout.
const LAYOUT_PRESETS: [(config::PaneSide, config::SplitDirection, u8); 4] = [
    (config::PaneSide::Left, config::SplitDirection::Horizontal, 50),
    (config::PaneSide::Right, config::SplitDirection::Horizontal, 50),
    (config::PaneSide::Left, config::SplitDirection::Horizontal, 70),
    (config::PaneSide::Left, config::SplitDirection::Vertical, 50),
];

/// Ask for a pane layout — the preset menu, falling through to `ask_layout_custom` on
/// "customize…" — and return the chosen (agent_pane, split, split_percent) triple, or `None`
/// if it exactly matches what's already effective (a cheap early exit; `update_global_tmux_
/// layout` still diffs per-key on top of this for the case where only one of three changed).
///
/// Not asked at all when `detected.global_config_path` is `None` (same rule as
/// `ask_container_enabled`) or when called under `--yes` (the caller skips it — see
/// `cmd_setup`).
pub fn ask_layout(
    io: &mut dyn Io,
    detected: &DetectedState,
) -> Result<Option<(config::PaneSide, config::SplitDirection, u8)>>;

/// The customize sub-flow: direction, then a direction-worded pane-side question, then
/// percent, then a preview-and-confirm. `Ok(Some(triple))` on confirmation; `Ok(None)` when
/// the preview is declined — the caller (`ask_layout`'s own loop) re-shows the preset menu in
/// that case rather than this function recursing into it, so repeated declines cannot grow the
/// call stack.
fn ask_layout_custom(
    io: &mut dyn Io,
    detected: &DetectedState,
) -> Result<Option<(config::PaneSide, config::SplitDirection, u8)>>;

/// A small fixed-width ASCII diagram, `PREVIEW_WIDTH` characters wide, proportioned by
/// `percent` and clamped so neither label is ever squeezed below its own length. Used both for
/// the four preset previews and for the one customize preview, so there is exactly one
/// rendering implementation to keep correct.
fn render_layout(
    agent_pane: &config::PaneSide,
    split: &config::SplitDirection,
    percent: u8,
) -> Vec<String>;

pub fn update_global_tmux_layout(
    path: &Path,
    agent_pane: config::PaneSide,
    split: config::SplitDirection,
    split_percent: u8,
) -> Result<Vec<&'static str>>;
```

### `main.rs::cmd_setup` — insertion point

The shipped function already asks the agent and container questions and writes their
answers; this revision inserts the layout question between them and the devcontainer note.
The agent/containers questions themselves are unchanged at the call site — the write-target
line is printed from inside `ask_agent`/`ask_container_enabled`, not by `cmd_setup`:

```rust
let container_answer = if yes { None } else { onboarding::ask_container_enabled(&mut io, &detected)? };
// ... existing agent/container write-back and confirmation printing ...

// New:
let layout_answer = if yes { None } else { onboarding::ask_layout(&mut io, &detected)? };
if let Some((agent_pane, split, split_percent)) = layout_answer {
    if let Some(path) = detected.global_config_path.as_deref() {
        let written = onboarding::update_global_tmux_layout(path, agent_pane, split, split_percent)?;
        if !written.is_empty() {
            println!("Set tmux.{} in {}", written.join(", tmux."), path.display());
        }
    }
}

// ... existing devcontainer note, doctor::run, next steps / first session unchanged ...
```

## Data model

Still no changes to `config::Config` or `session::Session` — every value `am setup` writes
was already a valid, parseable field before this feature existed (`TmuxConfig`'s three
fields, `ContainerConfig::enabled`, `defaults.agent`). The new types
(`DetectedState`'s three added `Effective<...>` fields, `TrackedTmux`, `LAYOUT_PRESETS`,
`WriteScope`) live entirely in `onboarding.rs` and exist only for the duration of one
`am setup` invocation, the same as the agent/container tracking that already shipped.

## Testing strategy

**Unit tests in `onboarding.rs`**, extending the existing `ScriptedIo`-based suite:

- `write_target_line` produces the two pinned strings ("... — just this repo; saved to
  .../config.toml." and "... — every repo on this machine; saved to .../config.toml.") for
  each `WriteScope` — one test, reused in spirit by every question that calls it.
- `ask_agent`'s captured output now includes the write-target line as its first line, and
  `ask_container_enabled`'s likewise — **these are updates to already-shipped tests, not new
  regressions**; anyone touching them should expect the diff and not "fix" it back.
- preset selection by number (1-4) returns the corresponding fixed triple; "5" (or
  "customize") enters the sub-flow
- the sub-flow's pane-side question is worded "left/right" after choosing horizontal and
  "top/bottom" after choosing vertical — this is the one behavior in this whole feature
  that's actually a correctness bug if it's wrong, so it gets a test that asserts on the
  literal prompt text, not just the resulting value
- accepting the customize preview returns the chosen triple; declining it re-shows the
  preset menu (assert the preset menu's text, including its write-target line, appears again
  in captured output)
- `render_layout` produces the expected diagram for each of the four fixed presets (pinned,
  exact-string tests, since there's no algorithm to fuzz there) and degrades sensibly at an
  extreme customize percentage (e.g. 95/5) without either label being squeezed to nothing
- `ask_layout` returns `None` when the chosen preset/customize result exactly matches
  `DetectedState`'s three effective values; returns `Some` otherwise
- `update_global_tmux_layout` on a file that already has all three keys correct returns an
  empty vec and does not touch the file's mtime; on a file that only disagrees on
  `split_percent` writes only that key and returns `["split_percent"]`, leaving any existing
  `agent_pane`/`split` lines (and their comments) untouched
- EOF at any of the three customize sub-questions aborts the same way every other question
  in the flow does
- the project-override caveat note is printed when (and only when) one of the three
  effective values' source is `Source::Project`

**Cucumber integration tests** (`tests/features/setup.feature`):

- any existing scenario that matches on `ask_agent`'s or `ask_container_enabled`'s prompt
  text (if the harness asserts against interactive-mode output anywhere, or against captured
  stdout for a scenario that happens to trigger a prompt before failing/aborting) needs its
  expected text updated for the new leading line — **flagged explicitly so this is treated
  as an intentional update alongside the feature, not a CI regression to chase separately**
- `am setup --yes` on a repo with no `[tmux]` anywhere → the global config file (freshly
  created or pre-existing) has no `[tmux]` keys added — locks in that layout is skipped, not
  silently defaulted, under `--yes`
- `am setup --yes` on a repo whose global config already sets a non-default layout → that
  file's mtime and bytes are unchanged (nothing about `--yes` should ever touch layout)

No new interactive cucumber coverage is added for the preset/customize prompts themselves —
same limitation as the existing agent/containers questions: the subprocess harness has no
seam for interactive stdin, so that logic is unit-tested only, per the bullets above.

## Task breakdown

Already shipped (the original agent/containers pass) is not repeated here. New work:

1. **backend-engineer** — `onboarding.rs`: `WriteScope`, `write_target_line`, and wiring it
   into `ask_agent` and `ask_container_enabled` as their first printed line. **This changes
   already-shipped prompt output** — update the existing unit tests that capture and assert
   on those two functions' output rather than treating the diff as a break.
2. **backend-engineer** — `onboarding.rs`: extend `TrackedKeys`/`resolve_effective` with
   `TrackedTmux` and the three new `DetectedState` fields, mirroring the existing
   `defaults`/`container` handling exactly.
3. **backend-engineer** — `onboarding.rs`: `LAYOUT_PRESETS`, `render_layout`, `ask_layout`
   (using `write_target_line` for its own header), `ask_layout_custom`,
   `update_global_tmux_layout`, plus the unit tests above.
4. **backend-engineer** — `main.rs::cmd_setup`: insert the layout question and its write-back
   between the containers question and the devcontainer note, per the insertion point above.
5. **integration-tester** — audit `tests/features/setup.feature` for any assertion against
   `ask_agent`/`ask_container_enabled` prompt text that the new leading line would break, and
   update it as part of this change (not as a separate bug); add the two new layout-under-
   `--yes` scenarios above.
6. **code-reviewer** — in addition to the existing review focus (no-secrets, exit codes,
   `toml_edit` correctness): confirm the write-target line reads identically across all three
   questions (same helper, not three hand-written near-duplicates); confirm the
   direction-first customize ordering never produces a "left/right" prompt after a vertical
   choice or a "top/bottom" prompt after a horizontal one; confirm
   `update_global_tmux_layout`'s per-key diffing genuinely avoids touching keys that didn't
   change.
7. **documentation-writer** — `docs/reference/commands.md`: update the `am setup` section's
   example flow to show the new question and the write-target line on all three prompts;
   note in the write-target summary table which file each question saves to.

## Edge cases & considerations

Carried over, still accurate: no secret ever transits a prompt or a config file; no
write-time race condition beyond the single-writer assumption already made everywhere else in
`am`; the `initializeCommand` gate is never auto-enabled by the wizard.

- **A project-level `tmux.*` override makes a global-config write for that repo
  not-immediately-visible.** Handled with the one-line caveat note described above — an
  explicit, cheap mitigation for an edge case that's expected to be rare (layout is framed
  throughout this spec as a global-only preference, so a project overriding it is already an
  unusual, deliberate act by definition).
- **Extreme customize percentages (e.g. 95/5) in the preview.** `render_layout`'s width
  clamp keeps both labels legible; this is a cosmetic best-effort, not a hard requirement —
  `am` already accepts and correctly applies any value in `tmux.split_percent`'s existing
  1-99 range regardless of how its preview renders.
- **Re-running `am setup` after picking "customize" once does not "remember" that the user
  came from customize** — the next run's default is simply whatever the three effective
  values now are, shown via the normal preset-menu "currently:" line (which may not match any
  of the four fixed presets, in which case none of [1]-[4] is visually marked as current
  beyond that line — no preset is falsely highlighted as selected when the saved layout is a
  genuine custom combination).
- **The write-target line is static per question, not per invocation.** It always says "just
  this repo" for agent and "every repo on this machine" for containers/layout, regardless of
  whether a global or project file happens to exist yet at the moment the question is asked
  (steps 2-3 guarantee both files exist by the time any question runs, so this is never
  actually ambiguous in practice — noted here only because the line's wording is fixed at
  compile time, not computed from `DetectedState`, and that's deliberate: the *scope* of a
  question never changes at runtime, only its default value does).

## Resolved Decisions

All decisions from the original Open Questions pass, plus both follow-up revisions.

1. **Command name: `am setup`.**
2. **Prompt crate vs. hand-rolled: hand-rolled**, behind the `Io` trait — chosen for
   testability against the `Io`/`ScriptedIo` seam, not for MSRV reasons (`am`'s actual floor
   is 1.88, not the 1.70 the docs claim; that doc staleness is tracked separately in
   BACKLOG.md).
3. **Relationship to `am init`: separate command**, calling `init`'s extracted logic
   (`init_project`) as its first step.
4. **Global config scope: narrow.** Originally `defaults.agent` (project) and
   `container.enabled` (global) only. The layout revision adds the three `tmux.*` keys (also
   global) as the third and last thing in scope. The line is still drawn deliberately: no
   network mode, container user, devcontainer settings, or `agents.<name>.image` — those
   remain hand-edit-only, because "layout is undetectable preference" is the specific
   justification for that addition and does not generalize to "more questions are better."
5. **Existing-config behaviour: show current values, offer to change them**, via a
   format-preserving `toml_edit` update rather than a from-scratch overwrite. The layout
   question follows the identical pattern: current effective value shown with source,
   accepting it is a no-op, a change is written with per-key granularity.
6. **Declining the first-session prompt still exits 0.**
7. **`--yes` does not start a session unattended.**
8. **Pane layout question — added in a follow-up revision, resolved with the user as
   follows:**
   1. **A single preset picker (4 presets + customize), not three granular questions
      up front.** Keeps the question short in the common case; granular control is one menu
      choice away, not the default path.
   2. **Written to the global config**, all three keys (`tmux.agent_pane`, `tmux.split`,
      `tmux.split_percent`) — layout is a personal habit, not a repo trait, so it belongs
      where the agent's own answer explicitly does *not* go.
   3. **The prompt states its write target** (superseded by decision #9 below, which
      generalized this to all three questions via the shared `write_target_line` helper
      rather than a layout-specific sentence).
   4. **Always asked (gated only on there being a global file to write to) — but unlike the
      agent question, never given a proactive best-guess write under `--yes`.** This
      asymmetry is deliberate: an unanswered agent is a functional gap (`am doctor` and
      `--auto` both care), an unanswered layout or `container.enabled` is not — both already
      fall back to a working compiled default with no consequence, so `--yes` treats them the
      same way (skip, no write) rather than treating layout like agent.
   5. **Customize asks direction before pane side**, wording the pane question as
      left/right for a horizontal split and top/bottom for a vertical one — the only ordering
      that produces a correctly worded question, since the pane question's wording is a
      function of the direction already chosen. The underlying config keys stay `left`/`right`
      regardless of which words the prompt uses; this is presentation only, not a schema
      change.
   6. **The "stacked" preset puts the agent on top**, matching `PaneSide::Left`'s existing
      role as `am`'s compiled default "first" side — agent-on-bottom remains reachable via
      customize rather than earning a fifth preset.
   7. **No new CLI flag for layout** (no `--layout`, no per-preset shortcut). Unlike
      `--agent`, there's no scripted-CI use case this addition is trying to serve — layout is
      squarely an interactive-only, one-time-per-machine preference, and `--yes` already
      handles the non-interactive path correctly by skipping it. Revisit only if a real
      request for a scriptable layout override shows up.
9. **Write-target legibility applies to all three questions uniformly — approved as a
   follow-up to #8.3.** Originally raised only for the layout question (the newest, and the
   one most obviously missing this), then generalized on the user's instruction to the agent
   and containers questions too, so the rule is "every question says where its answer will be
   saved," not a special case carved out for whichever question happened to be added most
   recently. Implemented as one shared `write_target_line` helper rather than three
   hand-written lines, specifically so the wording can't drift between them as the flow
   evolves further. The agent question's wording was called out as the one that needs care:
   it's the only question where the *displayed default's source* and the *write target* can
   differ (default from global, write always to project), so its prompt has to make both
   facts legible rather than leaning on the write-target line alone. This changes
   already-shipped output for `ask_agent` and `ask_container_enabled` — flagged explicitly in
   [Task breakdown](#task-breakdown) so the resulting test churn is understood as intentional.

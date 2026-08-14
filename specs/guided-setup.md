# Feature: Guided Setup (`am setup`)

A new, interactive command that walks a first-time or new-repo user through configuring
`am`, asking only the questions that detected state can't answer on its own, then verifying
the result and optionally launching a first session.

**Status:** the agent/containers flow described here shipped in PR #47. A follow-up revision
added a third question — pane layout — because the shipped flow, on a machine that already
has a container runtime installed, asked exactly one question (agent), which undershot the
user's original "make this as easy as possible" goal. That revision also extended all three
questions to state where their answer will be saved — see [Resolved
Decisions](#resolved-decisions) #9. **This revision** responds to on-ramp feedback that
`am setup` is "a strong guided configuration command, but not yet a complete first-time-user
on-ramp — it stops precisely where non-technical users need the most help." Five changes:
readiness (`doctor::run()`) now runs before the cosmetic layout question rather than after it;
a failing report ends with concrete remediation, not just a pointer back at itself; a brand-new
machine is asked, once, whether it wants isolated containers at all — not just told about a
missing runtime; the agent menu's credential language matches `am doctor`'s honesty; and the
docs' quick-start path leads with `am setup` instead of `am init`. Two of these reverse
behavior this document previously described — each is called out explicitly below and in
[Resolved Decisions](#resolved-decisions) #10 and #12, the same way #9 supersedes #8.3.

## Background

Today the on-ramp is `am init`: it creates `.am/config.toml` (fully commented out),
appends `.am/worktrees/` to `.gitignore`, and prints a short status report. It asks nothing and assumes
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
- **"Ask only what detected state can't answer" is about the answer, not just the input.**
  Whether a container runtime is installed *is* answerable by detection — but whether a
  first-time user wants their sessions containerised at all is a preference no probe can
  supply, the same category of gap the layout question fills for pane arrangement. This is
  why the containers question gets a second, informed-consent framing on a fresh setup — see
  [Resolved Decisions](#resolved-decisions) #12 — without weakening the rule for the cases
  where detection genuinely does answer the question (a returning setup, where consent was
  already given once).
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
   into the project file. Because nothing was found anywhere, the prompt also states the
   fallback explicitly rather than leaving it implicit — see [Resolved
   Decisions](#resolved-decisions) #13.
4. **The containers question, informed-consent framing.** No global config existed before
   this run, so `am setup` explains what containers are for and asks once, regardless of
   whether a runtime is currently installed: "Use isolated containers for your sessions?
   [Y/n]" — recommended, defaulted to yes. If no runtime was detected, one extra line notes
   that, without blocking the choice. Accepting the default writes nothing (containers are
   already the compiled default); declining writes `container.enabled = false` into the
   global file. **This is a behavior reversal from the originally shipped flow** — see
   [Resolved Decisions](#resolved-decisions) #12.
5. `am setup` runs the same checks `am doctor` runs and prints the report inline, **before**
   the layout question — see [Resolved Decisions](#resolved-decisions) #10.
   - **Clean (0 failures):** continue to step 6.
   - **Failures:** the report is followed by a "What to do next:" block listing each
     failure's remediation hint, and `am setup` exits with doctor's exit code. Steps 6-8 are
     not reached — see [Verification step](#verification-step).
6. The pane layout question — stating the same machine-wide framing as the containers
   question — is asked now that readiness is confirmed. A chosen preset or a customized
   combination is written into the global file.
7. `am setup` offers to start a first session.
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
4. **The containers question, returning-setup framing.** A global config already existed
   before this run, so consent was already asked (and given, implicitly or explicitly) in an
   earlier run — `am setup` does not ask again from scratch. It falls back to the original,
   failure-framed question, asked only if the trigger condition currently holds (no runtime
   found); if a previous run already resolved this, accepting its default writes nothing.
5. `am setup` runs doctor's checks and prints the report.
6. **If clean**, the layout question is still asked (it is not gated by "global config
   already sets one") — but its default is now the layout already saved globally, labeled
   `(from your global config)`. Pressing Enter through the preset menu (or picking the preset
   that happens to match) writes nothing.
7. Offer to start a first session, as UC1.

Once a user has a healthy global config, this still involves at most three real answers —
agent, containers, layout — each defaulting correctly, so a returning user who wants nothing
to change can press Enter through the agent and containers questions, see a clean report, and
press Enter once more for layout — same "press Enter through everything" tradeoff the
previous revision reached, just with the verification report now appearing in the middle
instead of at the end. See [Resolved Decisions](#resolved-decisions) #8 and #10.

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
4. The containers question runs in whichever framing applies (see [Question
   flow](#question-flow)) — for a user who already has a working global config from
   elsewhere, this is the returning-setup framing: if no runtime is currently found, it
   offers to disable containers so the user can keep working without one, or the user can
   leave it as-is and go install one.
5. `am setup` re-verifies with `doctor::run()` against whatever is now in the (possibly
   just-updated) project config and prints the report — reflecting any change just made in
   step 3-4.
   - **Clean:** the layout question runs — unrelated to whatever brought the user here, and
     asked on every clean run, not conditioned on there having been a problem to fix — then a
     first session is offered.
   - **Not clean:** the report's "What to do next:" block is the guidance; `am setup` stops
     there. Layout is not reached — the user is sent back to fix the readiness problem first,
     not asked to pick a pane arrangement for a session that can't start yet.

**Scope boundary, restated:** `am setup` can change the things it already knows how to ask
about — which agent, whether containers are enabled, and pane layout — on an existing
config. It still does **not** become a general repair tool: it never installs a container
runtime, never runs `gh auth login` or writes a token, and never edits any config key outside
`defaults.agent`, `container.enabled`, and the three `tmux.*` layout keys. The "What to do
next" block it prints on failure (see [Verification step](#verification-step)) is concrete
instructions, never an action taken on the user's behalf.

## Scope boundary vs. `am init` and `am doctor`

**A new, separate command — `am setup` — not a flag on `init`.** `am init` must stay fast,
silent, and scriptable: it's invoked from the cucumber fixtures and (per the docs) from setup
scripts as an idempotent, non-interactive primitive, and making it interactive by default
inverts that contract for every existing caller. This also matches the "fast path vs. custom
path" split BACKLOG.md already calls for: `am init` (fast, scriptable) and `am setup`
(guided, interactive) are the two doors. The docs' own quick-start path is being updated to
lead with `am setup` rather than `am init` — see [Resolved Decisions](#resolved-decisions)
#14 — but that changes which door a newcomer is pointed at first, not what either door does.

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
This revision extends that "cannot drift apart" principle one step further: the *hint* text
attached to a failing check is also shared, so strengthening a hint (a concrete install
command, a doc link) improves `am doctor` and `am setup`'s remediation block identically —
see [Verification step](#verification-step) and [Resolved Decisions](#resolved-decisions)
#11. Pane layout is not a doctor check (a "wrong" layout doesn't stop `am start` from
working), so it plays no part in the pass/fail verdict — but it *is* now gated on that
verdict: the layout question is only reached once `doctor::run()` reports zero failures. It
remains purely a preference question, just one that's deliberately asked after readiness is
confirmed rather than alongside it — see [Resolved Decisions](#resolved-decisions) #10.

This does **not** reverse the stance BACKLOG.md records for `am doctor` ("the alternative to
auto-bootstrapping `.am/` as a side effect of `am start`"). `am start` still does not
auto-bootstrap anything. `am setup` is an explicit, user-invoked command whose entire job
*is* bootstrapping — it's the opt-in front door, not a side effect.

## Question flow

The guiding rule remains **ask only what detected state can't answer** — with the refinement
recorded in [Assumptions](#assumptions): "the answer" sometimes means "whether the user wants
X at all," not just "what X's current value is," and that distinction is what justifies the
containers question's two framings below.

```
1. Preconditions (no prompt)
   ├─ not in a repo?              → same error as `am init`, exit
   └─ in a repo                   → proceed, VCS (git/jj) detected silently

2. Project config file (no prompt — an action, not a question)
   ├─ .am/config.toml missing     → write skeleton + .gitignore entry (= `am init`)
   └─ .am/config.toml exists      → open it, don't touch it yet — its values feed step 4

3. Global config file (no prompt — an action, not a question)
   ├─ ~/.config/am/config.toml missing  → write skeleton
   └─ ~/.config/am/config.toml exists   → open it, don't touch it yet — feeds steps 4-8

4. Agent question — ALWAYS asked, unless --agent or --yes was passed
   States: saves to THIS REPO's config. Default = effective value via project → global →
   compiled default, labeled with source. Accepting it writes nothing. A change writes
   `defaults.agent` into the PROJECT file only, regardless of which file the shown default
   came from. When nothing is configured anywhere and no agent has credentials found on this
   host either, the prompt states the fallback explicitly (see decision #13) rather than
   leaving it to be inferred from "currently: none configured."

5. Containers question — exactly one of two framings, chosen by whether a global config
   file already existed *before this run* (`detected.global_config_exists`, gathered prior
   to step 3's writes):

   5a. FRESH setup (no global config existed yet) → `ask_container_consent`. ALWAYS asked
       — regardless of whether a runtime is currently found — unless --yes or there is no
       global file to write to. States it saves MACHINE-WIDE, explains what containers are
       for, defaults to yes (recommended). If no runtime is found, one extra note says so
       without blocking the choice. Accepting the default writes nothing (matches the
       compiled default); declining writes `container.enabled = false`.

   5b. RETURNING setup (a global config already existed) → `ask_container_enabled`,
       unchanged from the originally shipped behavior: asked ONLY if neither podman nor
       docker is currently on PATH (and there is a global file to write to). Default =
       effective `container.enabled`, read from the GLOBAL file only. Accepting it writes
       nothing; a change writes `container.enabled` into the GLOBAL file.

   These two are mutually exclusive per run — `cmd_setup` calls exactly one, so a user is
   never shown both a "would you like containers?" question and a "no runtime found, proceed
   anyway?" question in the same invocation. See [Resolved Decisions](#resolved-decisions)
   #12 for why this is two functions rather than one with two framings.

6. Project-specific notes (no prompt — informational only)
   ├─ .devcontainer/devcontainer.json found → one line: "found — sessions will use it
   │    automatically (container.mode = auto)". No question: auto is already the correct
   │    default, and asking "use your devcontainer?" when the answer is obviously yes fails
   │    the "every question must justify itself" bar.
   │    If it also contains `initializeCommand` (which `am` refuses by default): a warning
   │    line pointing at `am doctor` and `devcontainer.allow_host_commands`, NOT a prompt —
   │    silently enabling host command execution from a wizard default is not acceptable.
   └─ none found                 → nothing printed; image mode is already the default

7. Verification (no prompt)
   → run doctor::run() against the resolved repo + agent (reflecting whatever was just
     written in steps 4-5), render exactly as `am doctor`
   → 0 failures  → continue to step 8
   → failures>0  → print the report, then a "What to do next:" block (see [Verification
     step](#verification-step)), exit with doctor's exit code. Steps 8-9 are not reached
     (see UC3).

8. Pane layout question — ALWAYS asked, unless --yes was passed, there is no global file to
   save the answer to (`detected.global_config_path` is `None`), OR step 7 found any
   failures. Moved here (after verification, not before it) in this revision — see
   [Resolved Decisions](#resolved-decisions) #10. States saves MACHINE-WIDE, same as
   containers. See the dedicated section below.

9. First session (prompt only if step 7 passed AND session is interactive, i.e. not --yes
   and stdin is a TTY)
   → "Start your first session now? [Y/n]"
        no / declined  → print next-step commands, exit 0
        yes            → "Session name: " (two tries, no default; falls through to
                           declining rather than looping forever on a required field)
                        → calls the same function `cmd_start` uses, with the resolved agent
```

**Why the layout question now runs after verification, not alongside agent/containers.**
Previously it was grouped with the other two prompts as "the third and last thing the user is
actually asked," on the theory that every interactive Q&A should happen together before the
flow moves into report-only territory. This revision deliberately breaks that grouping: agent
and containers are readiness questions (doctor cares about their answers), layout is not (a
"wrong" layout doesn't stop `am start` from working) — and asking a readiness-blind
personalisation question before confirming the machine can even run a session at all gets the
priorities backwards for a first-time user. Putting layout after a *clean* report means it is
only ever asked once the tool is confirmed to work, which is the point.

**Empty input, invalid input, Ctrl-C, EOF:** unchanged — accept the shown default on empty
input; re-ask with a one-line reason on invalid input, no retry limit; Ctrl-C is default
process behavior with no rollback needed (every write is independently idempotent); EOF
aborts with one message and a non-zero exit. The layout question's sub-flow (see below)
follows the same idiom at each of its own prompts. Note that the agent and containers writes
in steps 4-5 are **not** rolled back if verification (step 7) still fails for an unrelated
reason (e.g. missing git identity, or the newly-chosen agent's credentials also being
absent): they are real corrections the user asked for, and UC3 depends on this — a user
fixing their agent choice needs that fix to stick even if a second, unrelated problem is what
the report still flags.

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

/// One line, shared by `ask_agent`, `ask_container_enabled`, `ask_container_consent`, and
/// `ask_layout` — a single implementation so the wording cannot drift between them, and so
/// it's pinned by one set of tests instead of four copies that could disagree.
///
/// `base` is what `path` gets shortened against for display — `detected.repo_root` for
/// `WriteScope::Project`, `detected.home_dir` for `WriteScope::Global` — so a 80+ character
/// absolute project path doesn't wrap and defeat the point of an at-a-glance line. `None`
/// (no known base, or `path` isn't actually under it) falls back to the absolute path.
fn write_target_line(scope: WriteScope, path: &Path, base: Option<&Path>) -> String {
    let shown = shorten_for_display(scope, path, base);
    format!("{}; saved to {}.", scope.phrase(), shown.display())
}
```

Concrete output for each question (dimmed and indented two spaces — a readability pass moved
the label out of this line and into the question's own header, and put both this line and the
"currently: ..." line below it in the color-muted "structure, not content" treatment
[`color.rs`](../src/color.rs)'s module doc describes):

```
  just this repo; saved to .am/config.toml.
```
```
  every repo on this machine; saved to ~/.config/am/config.toml.
```

Each question now reads header first, write-target line second: the question's own line
("Which agent do you use?", "Use isolated containers for your sessions?", "Which layout do
you want?", ...) leads, this dim line comes right under it, then a blank line, then the
menu/prompt body, then a blank line, then the dim "currently: ..." line, then a blank line
before the prompt itself. The write-target line and the "currently:" line answer the user's
two questions in the order they'd ask them: "where would my answer go?" (this line) then
"where's the current default coming from?" (the `Source`-labeled line below the menu, which
may name a *different* file — see UC2) — that pairing is unchanged, only the header/body
ordering around them moved.

**This changed already-shipped output** in the layout revision, and stays unchanged again
here: existing assertions that pinned this text keep pinning it. `ask_container_consent` is
new, not a change to already-shipped output, but it reuses this exact line verbatim (the
`WriteScope::Global` phrase), so anything asserting on that literal string will match either
question — see [Testing strategy](#testing-strategy).

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

Checking your setup...

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
```

(the leading blank line is the phase separator every question opens with, printed before its
own header; the report and its verdict now appear directly above the layout question rather
than below it — the write-target and "currently:" lines are dimmed.)

Enter accepts the currently effective triple, not a hardcoded preset — the same "Enter means keep what's already in effect" idiom `ask_agent`'s and `ask_container_enabled`'s own defaults follow. On a first-time run with nothing configured, the effective triple happens to equal preset 1, but the prompt's wording does not assume that.

The write-target line is `write_target_line`'s output, described above — the same helper the
agent and containers questions now use, printed directly under this question's own header
("Which layout do you want?"), not a one-off sentence specific to layout.

**Customize sub-flow — direction first, then a direction-aware pane question, then
percent.** This ordering is the only one that produces correctly worded questions: the pane
question's wording (left/right vs. top/bottom) *depends on* the direction, so direction
cannot be asked second. Concretely:

```
8a. "Side by side, or stacked?
       [1] side by side (horizontal)   [2] stacked (vertical)"
     Default = current effective split, with source. → chosen direction

8b. Horizontal chosen: "Which side should the agent be on? [1] left  [2] right"
    Vertical chosen:   "Should the agent be on top or on the bottom? [1] top  [2] bottom"
    Default = current effective agent_pane, worded to match the chosen direction.
    → chosen side ("top"/"left" both map to PaneSide::Left; "bottom"/"right" to
      PaneSide::Right — the prompt's words change, the stored value's meaning does not)

8c. "What percentage of the window should the agent pane get? [1-99] (Enter for 50):"
    Default = current effective split_percent, with source. Out-of-range or non-numeric
    input re-asks, same as every other invalid-input case in this flow.

8d. Render the resulting layout with the same preview format as the preset menu, then:
    "Use this layout? [Y/n]"
      accepted → the (side, direction, percent) triple is the answer
      declined → back to the top of the layout question (the preset menu), not a partial
                 retry of 8a-8c — simpler to reason about, and customize is rare enough that
                 re-entering it is not a real cost
```

The write-target line is shown once, directly under the outer question's own header ("Which
layout do you want?"); the sub-questions don't repeat it — they're all still answering the one
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
  care about. The containers question — in **either** framing — and the layout question are
  both **skipped entirely under `--yes`**, writing nothing, because neither has an analogous
  "unanswered means broken" stake: an unset `container.enabled` and an unset `tmux.*` both
  fall back to a working compiled default with no functional consequence. This is a
  deliberate asymmetry, not an oversight — see [Resolved Decisions](#resolved-decisions)
  #8.4. Net effect: `am setup --yes` on an already-fully-configured repo writes nothing at
  all and degrades to "run doctor verification and print the report"; on a fresh repo it
  writes only what's needed for a session to actually start. Exit code **is doctor's exit
  code**, so `am setup --yes && am start feat --agent claude` is a valid CI bootstrap step.
  Step 9 (first session) never runs under `--yes` regardless of outcome, and since step 7
  always runs (verification is never skipped), the reordering in this revision has no
  observable effect on `--yes` output beyond what's already true above.
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
states this rule to the user in its own prompt (see above), so it's no longer only documented
here — it's legible at the point of decision. `ask_container_consent` writes through the
exact same `update_global_container_enabled` function `ask_container_enabled` already uses —
two questions, one write path, since both are answering the same underlying key.

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
// One key, one call each — already shipped. Called from both ask_container_enabled's and
// ask_container_consent's write-back — see above.
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

`am setup`'s verification is a direct call to `doctor::run(Some((&repo_root, vcs)),
agent_flag)`, run after the agent and containers writes (steps 4-5) have happened, rendered
with the existing `Report::render()`. **This revision moves the call earlier in the overall
flow** (before the layout question, not after it — see [Resolved Decisions](#resolved-decisions)
#10) but the call itself, and what it reads, are unchanged.

**On success**, it's followed by the layout question (step 8) and, ultimately, the "Next
steps" block (`print_next_steps` in `main.rs`).

**On failure**, `am setup` now prints a "What to do next:" block immediately after the
rendered report, then exits with doctor's exit code (1) — replacing the previous plain
"Fix the items above, then re-run 'am setup'." line. The block lists, in report order, the
hint attached to every `Status::Fail` check (`Status::Warn` checks are not included — they
don't block, and their hints are already visible inline in the report itself, right under the
`!` line they belong to):

```
Container runtime
  ✗ runtime        neither podman nor docker found on PATH
      → install Podman (https://podman.io/docs/installation) or Docker
        (https://docs.docker.com/get-docker/), or set container.enabled = false in
        .am/config.toml

Agent
  ✓ agent          claude
  ✗ credentials    ~/.claude does not exist
      → run 'claude auth login' (or set ANTHROPIC_API_KEY) — see
        docs/guides/claude-code.md#prerequisites

2 problems will prevent 'am start' from working.

What to do next:
  - install Podman (https://podman.io/docs/installation) or Docker
    (https://docs.docker.com/get-docker/), or set container.enabled = false in
    .am/config.toml
  - run 'claude auth login' (or set ANTHROPIC_API_KEY) — see
    docs/guides/claude-code.md#prerequisites

Then re-run 'am setup'.
```

No new remediation logic is written for `am setup` specifically — the block is exactly the
hints `doctor::Check` already carries, re-surfaced as a flat checklist. What *is* new is that
several of the shared hints in `doctor.rs` are strengthened, so both `am doctor` and
`am setup`'s new block benefit identically — this is the same "shared, not duplicated"
principle the rest of this module follows, extended to hint text:

- `check_agent`'s `Status::Fail` hint (missing credentials) now calls a new
  `container::credentials_hint(agent)`, giving a concrete, agent-specific command instead of
  the generic "authenticate `<agent>` on this machine": `claude auth login` (or
  `ANTHROPIC_API_KEY`) for Claude, `gh auth login` for Copilot, "authenticate with the Gemini
  CLI on this host" for Gemini, `codex` sign-in or `OPENAI_API_KEY` for Codex — each pointing
  at that agent's guide (`docs/guides/<agent>.md#prerequisites`) for the full explanation.
- `check_runtime`'s `Status::Fail` hint gains install links for Podman and Docker alongside
  the existing "or set `container.enabled = false`" escape hatch.
- `check_image_mode`'s `Status::Fail` hint gains a concrete example (`am setup --agent
  <name>`, or `defaults.agent = "..."` in `.am/config.toml`) instead of naming the keys
  abstractly.
- `check_project_setup`'s git-identity hint was already concrete (`git config --global
  user.name "Your Name"` etc.) and is unchanged — it's the model the other three now follow.

**Never auto-install, never auto-authenticate.** The remediation block only ever prints
instructions; `am setup` does not run `gh auth login`, does not install a container runtime,
and does not write a credential anywhere on the user's behalf — matching the existing
"not a repair wizard" boundary (see [Scope boundary](#scope-boundary-restated) in UC3).

## API / contract surface

This section matches the shipped module (`src/onboarding.rs`, `src/doctor.rs`,
`src/container.rs`, `main.rs::cmd_setup`) for everything except this revision's five changes.

### CLI (`src/cli.rs`) — unchanged

```rust
Setup {
    #[arg(short, long)]
    yes: bool,
    #[arg(short, long)]
    agent: Option<String>,
},
```

No new flag for layout or for the containers consent question — see [Resolved
Decisions](#resolved-decisions) #8.6; the same reasoning (interactive-only, one-time-per-
machine preference, `--yes` already the correct non-interactive path) applies to consent.

### `src/onboarding.rs`

```rust
pub enum Source { Project, Global, CompiledDefault }
pub struct Effective<T> { pub value: T, pub source: Source }

pub struct DetectedState {
    pub vcs: Option<config::Vcs>,
    pub repo_root: Option<PathBuf>,
    pub project_config_path: PathBuf,
    pub project_config_exists: bool,
    /// `None` only when neither `XDG_CONFIG_HOME` nor `HOME` is set. Gathered once, before
    /// any file this run creates — this is what makes it usable as the "was this fresh"
    /// signal for the containers question (see below): it always describes what existed
    /// *before* `am setup` started, never what steps 2-3 just wrote.
    pub global_config_path: Option<PathBuf>,
    /// Whether the global config existed **before this invocation's own writes**. Already
    /// used to decide whether to write a fresh skeleton (steps 2-3); this revision adds a
    /// second use — it is exactly the gate `cmd_setup` uses to choose between
    /// `ask_container_consent` (fresh) and `ask_container_enabled` (returning). See
    /// [Resolved Decisions](#resolved-decisions) #12.
    pub global_config_exists: bool,
    pub home_dir: Option<PathBuf>,
    pub tmux_present: bool,
    pub runtimes_found: Vec<container::RuntimeKind>,
    pub devcontainer: Option<PathBuf>,
    pub agent_credentials: Vec<(container::KnownAgent, bool)>,
    pub effective_agent: Effective<Option<container::KnownAgent>>,
    pub effective_container_enabled: Effective<bool>,
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

No new fields are needed on `DetectedState` for the consent question — `global_config_exists`
already existed and already meant exactly the right thing; this revision only adds a second
caller for it.

**New: the containers consent question**, alongside the unchanged `ask_container_enabled`:

```rust
/// Ask, on a fresh setup, whether the user wants sessions containerised at all — the
/// informed-consent framing. Unlike `ask_container_enabled`, this is not gated on whether a
/// runtime is currently found: that's the wrong question for a user who may not know
/// containers are involved at all. Only gated on there being a global file to write the
/// answer to (`detected.global_config_path` is `Some`) — same "nowhere to save it" rule
/// every other question in this module uses.
///
/// `cmd_setup` calls this only when `!detected.global_config_exists` and calls
/// `ask_container_enabled` otherwise — never both in the same run. See [Resolved
/// Decisions](#resolved-decisions) #12 for why these are two functions, not a shared one
/// with two framings.
pub fn ask_container_consent(
    io: &mut dyn Io,
    detected: &DetectedState,
    color: bool,
) -> Result<Option<bool>>;
```

Behavior: prints the header ("Use isolated containers for your sessions?"), the shared
`write_target_line(WriteScope::Global, ...)`, a short explanation (containers give each
session its own isolated filesystem/process sandbox; without them, sessions run directly on
the host), and — only when `detected.runtimes_found.is_empty()` — one additional dim note
that no runtime was found yet, without changing the default or blocking the answer. Default
is `[Y/n]`, i.e. "yes" (enabled) on empty input. Write semantics mirror the existing
`agent_write`/`layout_write` pattern: compares the chosen value against
`detected.effective_container_enabled.value` (always `CompiledDefault` / `true` on a
genuinely fresh setup, since nothing has set the key yet) and returns `None` when unchanged.
EOF aborts the same way every other question does.

**New, in `src/container.rs`** (alongside `validate_agent_credentials`, which it exists to
give a human-readable remediation for):

```rust
/// A concrete, agent-specific instruction for a credentials failure — presence-only, the
/// same guarantee `validate_agent_credentials` itself makes; never prints or implies
/// anything about whether the credentials found are still *valid*, only how to obtain some.
/// Used exclusively as `doctor::check_agent`'s `Status::Fail` hint.
pub fn credentials_hint(agent: KnownAgent) -> &'static str;
```

**Existing-file updates, unchanged from the layout revision:**

```rust
pub fn update_project_agent(path: &Path, agent: container::KnownAgent) -> Result<bool>;
pub fn update_global_container_enabled(path: &Path, enabled: bool) -> Result<bool>;
pub fn update_global_tmux_layout(
    path: &Path,
    agent_pane: config::PaneSide,
    split: config::SplitDirection,
    split_percent: u8,
) -> Result<Vec<&'static str>>;
```

**Wording change (no signature change):** the per-agent note in `ask_agent`'s menu changes
from `"authenticated"` to `"credentials found"`, matching `doctor::check_agent`'s own
`"present"` wording for the identical presence-only check — see [Resolved
Decisions](#resolved-decisions) #13. `ask_agent` also gains one additional printed line, shown
only when `detected.effective_agent.value.is_none()` and no entry in
`detected.agent_credentials` is `true` (i.e. the genuine "nothing found anywhere" case),
making the `claude` fallback explicit rather than leaving it to be inferred.

### `src/doctor.rs`

`Check` and `Report` are unchanged in shape — this revision only changes the *content* of
three `hint` strings (see [Verification step](#verification-step)):

- `check_agent`'s `Status::Fail` arm now builds its hint from `container::credentials_hint`
  instead of a generic `format!("authenticate {agent} on this machine, ...")`.
- `check_runtime`'s `Status::Fail` hint gains install links.
- `check_image_mode`'s `Status::Fail` hint gains a concrete example.

### `main.rs::cmd_setup` — reordering and new call sites

The layout question and its write-back move from between the containers question and the
devcontainer note to between the verification step and the first-session offer, gated on
`report.failures() == 0`. The containers call site branches on freshness instead of calling
`ask_container_enabled` unconditionally:

```rust
// Step 5 — was unconditional; now branches on whether a global config already existed.
let container_answer = if yes {
    None
} else if detected.global_config_exists {
    onboarding::ask_container_enabled(&mut io, &detected, color_enabled)?
} else {
    onboarding::ask_container_consent(&mut io, &detected, color_enabled)?
};
// ... existing container write-back and confirmation printing, unchanged ...

// ... existing devcontainer note (step 6), unchanged ...

// Step 7 — verification, moved earlier (was after the layout write-back).
println!("\nChecking your setup...\n");
let report = doctor::run(Some((repo_root.as_path(), vcs.clone())), agent_flag);
print!("{}", report.render(color_enabled));
if report.failures() > 0 {
    print_what_to_do_next(&report, color_enabled); // new: replaces the old one-line message
    std::process::exit(1);
}

// Step 8 — layout, moved here (was steps 4-5's neighbor, before verification).
let layout_answer = if yes {
    None
} else {
    onboarding::ask_layout(&mut io, &detected, color_enabled)?
};
if let Some((agent_pane, split, split_percent)) = layout_answer {
    if let Some(path) = detected.global_config_path.as_deref() {
        let written =
            onboarding::update_global_tmux_layout(path, agent_pane, split, split_percent)?;
        if !written.is_empty() {
            println!("{}", set_tmux_layout_line(&written, path, detected.home_dir.as_deref()));
            onboarding::strip_global_tmux_layout_examples(path, &written)?;
        }
    }
}

// ... existing step 9 (first session), unchanged ...
```

`print_what_to_do_next` is a small new formatter in `main.rs`, alongside
`set_container_enabled_line`/`set_tmux_layout_line`/etc.: it filters `report.checks` to
`Status::Fail`, prints a `"What to do next:"` heading, one `"  - {hint}"` line per check that
has one, and a closing `"Then re-run 'am setup'."` line — pure formatting, testable the same
way those sibling functions already are.

## Data model

Still no changes to `config::Config` or `session::Session` — every value `am setup` writes
was already a valid, parseable field before this feature existed (`TmuxConfig`'s three
fields, `ContainerConfig::enabled`, `defaults.agent`), and this revision writes no new keys —
`ask_container_consent` writes the same `container.enabled` `ask_container_enabled` always
has. The new types (`DetectedState`'s `Effective<...>` fields, `TrackedTmux`,
`LAYOUT_PRESETS`, `WriteScope`) live entirely in `onboarding.rs` and exist only for the
duration of one `am setup` invocation, unchanged by this revision. `doctor::Check`'s `hint`
field already existed; only its contents change for three checks.

## Testing strategy

**Unit tests in `onboarding.rs`**, extending the existing `ScriptedIo`-based suite:

- `write_target_line` produces the two pinned strings ("... — just this repo; saved to
  .../config.toml." and "... — every repo on this machine; saved to .../config.toml.") for
  each `WriteScope` — one test, reused in spirit by every question that calls it.
- preset selection, the customize sub-flow, `render_layout`, and `update_global_tmux_layout`:
  unchanged coverage from the layout revision, carried forward as-is.
- **`ask_container_consent`**: prints the write-target line and an explanation; when
  `runtimes_found` is empty, includes the "no runtime found yet" note; when non-empty, omits
  it; accepting the default (`[Y/n]`, empty input) writes nothing; declining writes
  `Some(false)`; EOF aborts; no `global_config_path` → `Ok(None)`, no output.
- **the menu's "credentials found" wording**: replaces the prior pinned assertion on
  `"authenticated"` — an intentional update, not a break, called out the same way the
  write-target-line change was in the previous revision.
- **the explicit-fallback note**: present when nothing is configured anywhere and no agent
  has credentials found for it; absent otherwise (e.g. when at least one agent's credentials
  are found, even if none is configured — that case already prints `"Enter for <agent>"`,
  which is explicit enough).

**Unit tests in `doctor.rs`:**

- `check_agent`'s `Status::Fail` hint, per agent, contains that agent's concrete command
  (`"claude auth login"` for Claude, `"gh auth login"` for Copilot, etc.) — not the old
  generic "authenticate `<agent>`" text.
- `check_runtime`'s `Status::Fail` hint contains both install links.
- `check_image_mode`'s `Status::Fail` hint contains the concrete `am setup --agent` /
  `defaults.agent` example.

**Unit tests in `main.rs`:**

- `print_what_to_do_next` (or equivalent formatter): given a report with a mix of `Fail` and
  `Warn` checks, only the `Fail` hints appear, each as its own `"  - "`-prefixed line, in
  report order; given a report with zero `Fail` checks, prints nothing (this function is only
  ever called from the failure branch, but it should still degrade sensibly if that changes).

**Cucumber integration tests** (`tests/features/setup.feature`):

- any existing scenario that matches on `ask_agent`'s, `ask_container_enabled`'s, or the
  failure ending's exact text needs updating for the "credentials found" wording and the new
  "What to do next:" block — **flagged explicitly so this is treated as an intentional update
  alongside the feature, not a CI regression to chase separately**.
- **ordering:** on a repo where verification fails (e.g. no agent credentials anywhere and no
  `--agent`), the layout question's header ("Which layout do you want?") never appears in
  captured output, and the run exits non-zero before reaching it.
- **ordering, clean case:** on a repo where verification passes, the "Checking your setup..."
  / "Ready." text appears in captured output *before* "Which layout do you want?" — pinning
  the new order, not just its presence.
- **remediation:** on a failing repo, captured output contains a "What to do next:" heading
  followed by at least one `"  - "` line, and does not contain the old
  "Fix the items above, then re-run 'am setup'." sentence.
- **consent, fresh + runtime present:** on a repo with no prior global config and a mock
  runtime on `AM_PODMAN_BIN`, `am setup` prints "Use isolated containers for your sessions?"
  and does **not** print "No container runtime found on this machine" (the old failure-framed
  header) — confirming the two framings are mutually exclusive at the call site.
- **consent, fresh + no runtime:** same repo, no mock runtime configured — the consent
  question still appears (proving it isn't gated on runtime absence), plus the "no runtime
  found yet" note.
- **returning setup unaffected:** on a repo with a pre-existing global config and no runtime,
  the original "No container runtime found on this machine (neither podman nor docker)."
  header still appears, unchanged.
- `am setup --yes` scenarios from the layout revision (no `[tmux]` written; a pre-existing
  non-default layout untouched) are carried forward unchanged — reordering step 7 earlier
  doesn't change anything about a run that never reaches steps 5 or 8 in interactive form
  regardless.

No new interactive cucumber coverage is added for the preset/customize prompts or the
consent question's own body — same limitation as the existing agent/containers questions: the
subprocess harness has no seam for interactive stdin, so that logic is unit-tested only, per
the bullets above.

## Task breakdown

Already shipped (the original agent/containers pass, and the pane-layout revision above) is
not repeated here. New work, for this on-ramp revision:

1. **backend-engineer** — `main.rs::cmd_setup`: move the layout question and its write-back
   from before verification to after it, gated on `report.failures() == 0`; replace the
   "Fix the items above..." line with a call to a new `print_what_to_do_next` formatter (see
   [API / contract surface](#api--contract-surface)).
2. **backend-engineer** — `onboarding.rs`: `ask_container_consent`, using the shared
   `write_target_line`/`dim_line` helpers; wire `cmd_setup`'s containers call site to branch
   on `detected.global_config_exists` between it and the existing `ask_container_enabled`.
3. **backend-engineer** — `doctor.rs` + `container.rs`: add `container::credentials_hint`
   and use it in `check_agent`'s `Status::Fail` arm; strengthen `check_runtime`'s and
   `check_image_mode`'s `Status::Fail` hints with concrete commands/links, per [Verification
   step](#verification-step).
4. **backend-engineer** — `onboarding.rs`: change `ask_agent`'s per-agent menu note from
   `"authenticated"` to `"credentials found"`; add the explicit-fallback note when nothing is
   configured anywhere and no agent has credentials found. **This changes already-shipped
   prompt output** — update the existing pinned test rather than treating the diff as a
   break.
5. **integration-tester** — audit `tests/features/setup.feature` for text broken by items 1
   and 4; add the ordering, remediation, and consent-question scenarios listed in [Testing
   strategy](#testing-strategy).
6. **code-reviewer** — confirm the layout question is gated on `report.failures() == 0`
   specifically (not "zero checks of any status," which would also block it on a mere
   warning); confirm `ask_container_consent` and `ask_container_enabled` are never both
   reachable in the same `cmd_setup` run; confirm no hint string added in items 3-4 ever
   embeds a secret or credential value, only instructions; confirm "credentials found"
   reads identically to `doctor::check_agent`'s own "present" framing in intent, even though
   the literal words differ (menu vs. report context justify the difference — see decision
   #13).
7. **documentation-writer** — `docs/reference/commands.md`: update the `am setup` example
   flow for the new question order and the "What to do next" block.
   `docs/getting-started/quick-start.md`: make `am setup` Step 1, retaining `am init` as a
   later, clearly-labelled fast/scriptable-path step — matching the README's existing
   framing. Docs-only; no code change — see [Resolved Decisions](#resolved-decisions) #14.

## Edge cases & considerations

Carried over, still accurate: no secret ever transits a prompt or a config file; no
write-time race condition beyond the single-writer assumption already made everywhere else in
`am`; the `initializeCommand` gate is never auto-enabled by the wizard; a project-level
`tmux.*` override gets a one-line caveat before the layout prompt; extreme customize
percentages degrade the preview cosmetically without affecting the stored value; re-running
`am setup` after a customize answer does not "remember" having come from customize; the
write-target line's wording is fixed per question, not computed from whether either file
happens to exist yet (steps 2-3 guarantee both do by the time any question runs).

New in this revision:

- **Agent/container writes are not rolled back when verification still fails for a different
  reason.** If a user fixes their agent choice in step 4 but the report still fails on, say,
  missing git identity, `defaults.agent` stays changed — that's a real, independently correct
  fix the user asked for, not a draft contingent on everything else also passing. This was
  already true before this revision (verification always ran after every write); reordering
  the *layout* question doesn't change it, since layout was never part of the verification
  precondition to begin with.
- **A user on a repo with a readiness problem is never shown the layout question until they
  fix it and re-run `am setup` successfully.** This is the intended effect of [Resolved
  Decisions](#resolved-decisions) #10, not a bug — personalisation is deliberately deferred
  behind readiness, every time, not just on the first run.
- **"Fresh," for the containers question, is defined by the global config file, not the
  project one.** A repo that already has `.am/config.toml` (e.g. from an earlier `am init`)
  but whose user has never run `am setup` anywhere and so has no
  `~/.config/am/config.toml` yet is still "fresh" for this question — `container.enabled` is
  a global-scope key, and no prior run has ever asked for consent on this machine, regardless
  of what the project file contains. `DetectedState::gather` already captures
  `global_config_exists` before this run's own writes, so no new plumbing is needed to make
  this distinction precisely.
- **Defaulting to "yes" on a fresh setup with no runtime installed leaves
  `container.enabled = true` and a subsequent doctor failure.** This looks like a new risk
  but is not: `ask_container_enabled`'s own pre-existing default (accepting `[y/N]` on an
  already-`true` effective value) already keeps containers enabled by default even with no
  runtime present, on the theory that the user should be told to go install one rather than
  have their preference silently overridden. `ask_container_consent` inherits the identical
  outcome for the same reason — the failing runtime check, and its now-concrete remediation
  hint (see [Verification step](#verification-step)), is precisely how the user finds out
  what to do next.
- **The layout question's own gate list grew by one condition** (`report.failures() == 0`,
  in addition to `--yes` and "no global file to write to") but its own body, defaults, and
  write behavior are unchanged — only *when* it's reached moved, not *what* it asks or does
  once reached.

## Resolved Decisions

All decisions from the original Open Questions pass, the pane-layout revision, and this
on-ramp revision.

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
      same way (skip, no write) rather than treating layout like agent. **Extended by decision
      #10 below**: as of this revision, "always asked" also implicitly means "once
      verification has passed" — the `--yes` skip described here is unaffected.
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
   facts legible rather than leaning on the write-target line alone. This changed
   already-shipped output for `ask_agent` and `ask_container_enabled`, and now extends to
   `ask_container_consent` too, since it's new and reuses the same helper from the start.
10. **Readiness ordering: `doctor::run()` now runs before the layout question, not after
    it — REVERSES the placement this document previously described.** New flow: agent →
    containers → `doctor::run()` → (only if clean) layout → first session. Previously,
    layout was grouped with agent and containers as "the third and last thing the user is
    actually asked" before verification ran; that grouping's own rationale (in [Question
    flow](#question-flow), now rewritten) is superseded by this decision. Agent stays first
    because doctor validates the *selected* agent's credentials, so an agent has to be chosen
    before verification is meaningful; containers is itself a readiness matter (its answer
    determines whether the runtime check even applies), so it also precedes doctor. Layout,
    having no bearing on doctor's verdict, is deferred until after a clean report — asking a
    first-time user "agent on the left, 70/30?" before confirming the tool can run a session
    at all puts cosmetic personalisation ahead of "does this work," which is the wrong
    priority for someone who most needs to know the answer to the second question. `--yes` is
    unaffected: layout (and, per #8.4, the containers question) were already skipped there
    regardless of order.
11. **Remediation: a "What to do next" block on failure, built entirely from doctor's own
    (now strengthened) hints — no separate remediation system.** The prior failure ending
    ("Fix the items above, then re-run 'am setup'.") gave no path forward beyond re-reading
    the report. Considered and rejected: a remediation system living inside `am setup` itself
    — rejected because it would necessarily duplicate or drift from `doctor`'s own check/hint
    logic, exactly the coupling `am setup` calling `doctor::run()` directly (rather than
    reimplementing checks) already exists to prevent. Instead: `doctor.rs`'s existing
    per-check `hint` field is strengthened for the checks that most block a first-time user
    (missing runtime, missing credentials, no image configured) with concrete commands and
    doc links, and `am setup` adds a small formatter that re-lists every failing check's hint
    as a flat, scannable "What to do next:" checklist after the report. Because the hints
    themselves live in the shared `doctor.rs`, `am doctor` gets the identical improvement for
    free. Never auto-installs or auto-authenticates anything — instructions only, matching
    the existing "not a repair wizard" boundary.
12. **Container choice becomes an explicit, informed-consent question on a fresh setup —
    REVERSES the early-return behavior `ask_container_enabled` has had since it first
    shipped.** That function's own doc comment states the prior reasoning verbatim: "with a
    runtime present there is nothing ambiguous to resolve." This decision narrows, rather
    than discards, that reasoning: whether a runtime is *installed* is genuinely answerable
    by detection, but whether the user *wants* containerised sessions at all is not — a
    newcomer may not know sessions run in containers, or that host-only execution is even an
    option, independent of what happens to be on PATH. Scope: asked only on a fresh setup
    (`!DetectedState::global_config_exists`, captured before this run's own writes),
    recommended and defaulted to yes, not re-asked on any later run once a global config
    exists (whether created by answering this very question or by anything else).
    Considered and rejected: merging this into `ask_container_enabled` as a second framing
    selected by an internal branch — rejected because the two questions' preconditions
    (gated on runtime absence vs. always asked), wording (failure-framed vs.
    consent-framed), and defaults differ enough that a merged function's own branching would
    obscure the exact distinction this decision draws, and because every other question in
    this module is already its own top-level function, a precedent worth keeping. Implemented
    instead as two functions — `ask_container_consent` (new) and `ask_container_enabled`
    (unchanged) — called mutually exclusively from `cmd_setup` based on
    `detected.global_config_exists`, so a user is never shown both a "would you like
    containers?" question and a "no runtime found, proceed anyway?" question in one run.
13. **Presence language, and a more explicit fallback.** The agent menu's per-agent note
    changes from "authenticated" to "credentials found," matching `doctor::check_agent`'s own
    "present" wording for the identical presence-only check (`validate_agent_credentials`
    checks that required files or environment variables exist, never that the credentials
    they hold still work). When nothing is configured anywhere and no agent has credentials
    found for it on this host either, `ask_agent` now states the `claude` fallback explicitly
    rather than leaving a user to infer it from "currently: none configured" plus an
    `Enter for claude` prompt alone.
14. **Docs: `am setup` becomes quick-start's Step 1; `am init` is retained as the later,
    scriptable-path step — docs-only, tracked for the documentation-writer.**
    `docs/getting-started/quick-start.md`'s Step 1 currently walks a newcomer through
    `am init` and hand-editing `.am/config.toml`, which is exactly the manual-configuration
    burden `am setup` exists to remove — and contradicts the README, which already leads with
    `am setup`. No code changes; `am init` keeps its existing behavior and its existing
    audience (the cucumber fixtures, and users who already know what they want — see decision
    #3), just demoted to a later, clearly-labelled step in this one doc.

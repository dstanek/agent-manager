#!/usr/bin/env bash
#
# Run one real `am attach` container recreate against a real tmux server and a real container
# runtime — the one combination the ordinary suite mocks. This exercises the exact code path
# the `respawn_pane` / `send_keys` fix changed (see the doc comments on `tmux::send_keys` and
# `tmux::respawn_pane`, and `launch_into_agent_pane` in src/main.rs): the devcontainer relaunch
# script (`container::user_env_probe_script`) contains literal embedded newlines, and this
# forces it through a real pty rather than a mock that just logs its argv.
#
#   ./scripts/test-live-attach.sh
#
# IMPORTANT CAVEAT, recorded here rather than left implied: this script proves the *positive*
# path — that `respawn_pane`'s `$SHELL -c` delivery gets the whole multi-line script through
# intact and `postAttachCommand` (the last thing the generated script runs) actually executes.
# It does **not** reliably reproduce the originally diagnosed failure mode of the bug it guards
# against. Investigating that directly (real tmux 3.7c + zsh, on this host): typing the exact
# same multi-line single-quoted script via `tmux send-keys` — both by hand and via a
# hand-reverted `am` binary — completed and ran to the end every time, including with a single
# line over 4KB (past the kernel's canonical-mode MAX_CANON) preceding the first embedded
# newline. The `quote>` continuation prompts appear exactly as they would if a user typed a
# multi-line single-quoted command interactively, and the shell reassembles and runs the whole
# thing once the quote closes — it does not submit a fragment mid-quote on this host. Whatever
# originally produced "nothing executed" was not reproduced here; it may be specific to a
# different shell/prompt configuration, a race in how a *freshly split, not-yet-ready* pane
# accepts its first input, or something else the debugger should account for. `respawn_pane` is
# still strictly the more robust delivery mechanism regardless (it hands the command to the
# shell in one argv element via `-c`, with no dependence on pty canonical-mode timing at all),
# so this script is worth keeping as a positive-path regression guard even though it cannot
# currently be flipped red against the pre-fix code on this host.
#
# Requires: a container runtime and tmux. Deliberately uses `container.mode = "devcontainer"`
# with a plain `image` (no dockerComposeFile), which `am`'s native builder can build with just
# the runtime binary — no reference devcontainer CLI, no compose provider, so this is the one
# live-session script that runs anywhere `scripts/test-live-session.sh`'s compose requirement
# would rule out (e.g. a podman-only host with no real Compose v2).
#
# Marker placement: `postAttachCommand` writes into the workspace folder rather than leaving a
# trace inside the container and reading it back with `exec`. The agent here (`sh`, chosen so
# no credential preflight applies — see the config.toml note below) does not reliably keep the
# container alive under `-it` through this script's nested tmux/podman path, and the
# container is `--rm`, so anything written only inside it can vanish before it's checked. The
# workspace folder is bind-mounted at the same path inside and outside the container (see
# `container::resolve_mounts`), so a file written there survives the container exiting.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH="${AM_LIVE_TEST_DIR:-$REPO/target/live-attach}"
SLUG="live-attach-feature"
SOCKET="am-live-attach-test"
AM="$REPO/target/debug/am"
MARKER="$SCRATCH/.am/worktrees/$SLUG/.am-post-attach-marker"

runtime() {
    for candidate in podman docker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            echo "$candidate"
            return
        fi
    done
    echo "error: no container runtime found" >&2
    exit 1
}
RT="$(runtime)"

cleanup() {
    tmux -L "$SOCKET" kill-server 2>/dev/null || true
}
trap cleanup EXIT

# Runs a command in the tmux session and waits for it, because `am` refuses to run outside tmux
# — it has a window to create. The exit status comes back through a file; `send-keys` reports
# only whether the keystrokes were delivered.
in_tmux() {
    local label="$1" command="$2" rc="$SCRATCH/$1.rc"
    rm -f "$rc"
    tmux -L "$SOCKET" send-keys -t "$DRIVER" \
        "cd '$SCRATCH' && $command; echo \$? > '$rc'" Enter
    for _ in $(seq 120); do
        [ -f "$rc" ] && break
        sleep 1
    done
    if [ ! -f "$rc" ]; then
        echo "FAIL: '$label' did not finish within two minutes" >&2
        tmux -L "$SOCKET" capture-pane -p -S -400 -t "$DRIVER" >&2 || true
        exit 1
    fi
    if [ "$(cat "$rc")" != "0" ]; then
        echo "FAIL: '$label' exited $(cat "$rc")" >&2
        tmux -L "$SOCKET" capture-pane -p -S -400 -t "$DRIVER" >&2 || true
        exit 1
    fi
}

echo "==> building am"
cargo build --manifest-path "$REPO/Cargo.toml" >/dev/null

echo "==> preparing $SCRATCH"
tmux -L "$SOCKET" kill-server 2>/dev/null || true
# A previous run's rootless podman storage under $SCRATCH/home is owned by subuids a plain `rm
# -rf` (running as this uid, outside the user namespace) cannot remove; `podman unshare` enters
# that namespace, where they're visibly this user's own files again.
if [ "$RT" = "podman" ] && [ -d "$SCRATCH/home/.local/share/containers" ]; then
    podman unshare rm -rf "$SCRATCH" 2>/dev/null || rm -rf "$SCRATCH"
else
    rm -rf "$SCRATCH"
fi
mkdir -p "$SCRATCH/.devcontainer" "$SCRATCH/home"
# An empty .zshrc so a scratch $HOME doesn't trip zsh's newuser-install wizard, which eats the
# first keystrokes of whatever's sent to the pane while it's waiting at its own prompt.
touch "$SCRATCH/home/.zshrc"

# No dockerComposeFile: a plain image, built natively, is the minimal devcontainer config that
# still produces a multi-line relaunch script (userEnvProbe defaults on, plus the injected
# postAttachCommand hook) — the compose path is a separate run model this script isn't after.
cat > "$SCRATCH/.devcontainer/devcontainer.json" <<'JSON'
{
  "name": "live-attach",
  "image": "debian:bookworm-slim",
  "postAttachCommand": "touch .am-post-attach-marker"
}
JSON

git -C "$SCRATCH" init -q
git -C "$SCRATCH" add -A
git -C "$SCRATCH" -c user.name=am -c user.email=am@example.com \
    commit -qm "devcontainer" --no-verify

# `agent = "sh"` is deliberately not one of am's known agents: `agent_command` degrades an
# unrecognized name to a bare command (see `agent_command_degrades_to_a_bare_command_for_an_
# unrecognized_name` in main.rs), which skips `container::preflight_agent_auth` entirely — this
# script tests the relaunch delivery mechanism, not any agent's credentials.
mkdir -p "$SCRATCH/.am"
cat > "$SCRATCH/.am/config.toml" <<'TOML'
agent = "sh"

[container]
mode = "devcontainer"
TOML

echo "==> starting a tmux server"
# HOME is set on the server itself, not inline in the driver's command: a split/new pane
# inherits the environment tmux's *server* process started with, not whatever a sibling pane's
# shell happened to export — an inline `HOME=... command` in the driver pane never reaches a
# pane tmux spawns independently, so a later podman invocation in a freshly split pane would
# read the real $HOME's image store instead of this scratch one.
env HOME="$SCRATCH/home" tmux -L "$SOCKET" new-session -d -s live -c "$SCRATCH"
# Keep the pane id: `am start`/`am attach` open their own window and make it current, so
# anything sent to the session by name after that lands in the agent's pane instead of this
# driver shell.
DRIVER="$(tmux -L "$SOCKET" display-message -p -t live '#{pane_id}')"

echo "==> am start"
in_tmux start "$AM start $SLUG"

if [ ! -f "$MARKER" ]; then
    echo "FAIL: postAttachCommand's marker did not appear after am start" >&2
    exit 1
fi
rm -f "$MARKER"

echo "==> simulating the window and the container both being gone"
# The window: exactly OQ-2/OQ-6's trigger for recreate_attach_window's container branch.
tmux -L "$SOCKET" kill-window -t "am-${SLUG}" 2>/dev/null || true
# The container: `am`'s `sh` agent does not reliably stay attached to the pane's pty through
# this nested tmux/podman path, so the `--rm` container from `am start` is typically already
# gone by this point; remove it explicitly too so the recreate's deterministic `--name` can
# never collide with a leftover.
CONTAINER_NAME="$(HOME="$SCRATCH/home" "$RT" ps -a --format '{{.Names}}' | grep "^am-${SLUG}" || true)"
if [ -n "$CONTAINER_NAME" ]; then
    HOME="$SCRATCH/home" "$RT" rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
fi

echo "==> am attach (recreate path)"
in_tmux attach "$AM attach $SLUG"

echo "==> waiting for the recreated container's postAttachCommand marker"
found=0
for _ in $(seq 30); do
    if [ -f "$MARKER" ]; then
        found=1
        break
    fi
    sleep 1
done
if [ "$found" != "1" ]; then
    echo "FAIL: $MARKER never reappeared after am attach recreated the container" >&2
    echo "--- agent pane content ---" >&2
    tmux -L "$SOCKET" capture-pane -p -t "am-${SLUG}" >&2 || true
    exit 1
fi
echo "    postAttachCommand ran — the whole multi-line relaunch script executed intact"

echo
echo "PASS"

#!/usr/bin/env bash
#
# Run one real session end to end: a compose project brought up by `am start`, an agent execed
# into its service, and `am destroy` taking it down again.
#
#   ./scripts/test-live-session.sh
#
# Why this is a script and not a test: it needs a container runtime, a tmux server and a live
# agent process at the same time — the one combination the ordinary suite mocks, because mocking
# any part of it is what makes the suite fast. Everything the mocks cannot tell us lives here:
# that the override document compose actually accepts is the one `am` writes, that the agent's
# service is the one it execs into, and that a destroy leaves nothing behind.
#
# Requires: a container runtime with a working `compose`, tmux, and a checkout whose path is the
# same inside and outside any container (see "Paths" below).
#
# ── Paths ─────────────────────────────────────────────────────────────────────────────────
#
# `am` bind-mounts the session worktree by its host path. Inside a dev container using
# docker-outside-of-docker that only works where the checkout is mounted at the *same* path it
# has on the host — which is how `am`'s own dev container is set up, so the scratch repo is
# created under `target/` rather than in /tmp, where the host daemon would find nothing.
#
# The same reasoning applies to `$HOME`: this container's home is nowhere on the host, so the
# session runs with a scratch HOME under the scratch repo. Without it the daemon fails trying to
# invent `/home/<user>` for the generated gitconfig mount.
#
# One thing does *not* survive the nesting: writing to the bind-mounted worktree from the session
# container. Under a rootless runtime the files this container creates belong to a subuid on the
# host, so a sibling container running as your uid cannot write them. Nothing here depends on
# that — the assertions read container state — but a hook that writes into the worktree will fail
# when the harness is run this way and not when it is run on the host.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH="${AM_LIVE_TEST_DIR:-$REPO/target/live-session}"
SLUG="live-feature"
PROJECT="am-${SLUG}"
SOCKET="am-live-test"
AM="$REPO/target/debug/am"

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
    # Belt and braces: the test asserts `am destroy` did this, but a failure part-way through
    # must not leave a project running.
    "$RT" ps -aq --filter "label=com.docker.compose.project=${PROJECT}" \
        | xargs -r "$RT" rm -f >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Runs a command in the tmux session and waits for it, because `am` refuses to run outside tmux
# — it has a window to create. The exit status comes back through a file; `send-keys` reports
# only whether the keystrokes were delivered.
in_tmux() {
    local label="$1" command="$2" rc="$SCRATCH/$1.rc"
    rm -f "$rc"
    # A scratch HOME, because every path am mounts has to exist on the *host* daemon's
    # filesystem. Under docker-outside-of-docker this container's real home does not — its
    # /home/vscode is nowhere on the host, so mounting the generated gitconfig from there fails
    # with a permission error while the daemon tries to invent the directory.
    tmux -L "$SOCKET" send-keys -t "$DRIVER" \
        "cd '$SCRATCH' && HOME='$SCRATCH/home' $command; echo \$? > '$rc'" Enter
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

compose_containers() {
    "$RT" ps -q --filter "label=com.docker.compose.project=${PROJECT}" | grep -c . || true
}

echo "==> building am"
cargo build --manifest-path "$REPO/Cargo.toml" >/dev/null

echo "==> preparing $SCRATCH"
cleanup
rm -rf "$SCRATCH"
mkdir -p "$SCRATCH/.devcontainer" "$SCRATCH/home"

# `shutdownAction: "none"` because there is no real agent here: the configured one is not in a
# debian-slim service, so it exits at once, and the default `stopCompose` would then take the
# project down while the assertions were still reading it. `am destroy` takes it down explicitly
# regardless, which is the thing worth asserting.
#
# The hook's trace goes inside the container rather than into the bind-mounted worktree: under a
# rootless runtime with a user namespace, a sibling container running as your uid cannot write
# files the *host* sees as a subuid, so a worktree write fails for reasons that have nothing to
# do with am. Checking both services instead is a stronger assertion anyway — it says which
# service was execed into, not merely that something ran.
cat > "$SCRATCH/.devcontainer/devcontainer.json" <<'JSON'
{
  "name": "live",
  "dockerComposeFile": "docker-compose.yml",
  "service": "app",
  "shutdownAction": "none",
  "postCreateCommand": "touch /tmp/am-post-create"
}
JSON

# Two services, so the test can tell "brought the project up" from "started one container", and
# `sleep infinity` because keeping a service alive is the compose file's job — am execs into it.
cat > "$SCRATCH/.devcontainer/docker-compose.yml" <<'YAML'
services:
  app:
    image: debian:bookworm-slim
    command: sleep infinity
  db:
    image: debian:bookworm-slim
    command: sleep infinity
YAML

git -C "$SCRATCH" init -q
git -C "$SCRATCH" add -A
git -C "$SCRATCH" -c user.name=am -c user.email=am@example.com \
    commit -qm "devcontainer" --no-verify

# The agent is `sh`: this is a test of the session, not of any agent, and every image has one.
mkdir -p "$SCRATCH/.am"
cat > "$SCRATCH/.am/config.toml" <<'TOML'
agent = "claude"

[container]
mode = "devcontainer"
TOML

echo "==> starting a tmux server"
tmux -L "$SOCKET" new-session -d -s live -c "$SCRATCH"
# Keep the pane id: `am start` opens its own window and makes it current, so anything sent to
# the session by name after that lands in the agent's pane instead of this driver shell.
DRIVER="$(tmux -L "$SOCKET" display-message -p -t live '#{pane_id}')"

echo "==> am start"
in_tmux start "$AM start $SLUG"

count="$(compose_containers)"
if [ "$count" -lt 2 ]; then
    echo "FAIL: expected both services of $PROJECT to be running, found $count" >&2
    "$RT" ps --filter "label=com.docker.compose.project=${PROJECT}" >&2
    exit 1
fi
echo "    both services are up"

# Polled, because `am start` returns once the agent pane is launched, not once the hook chained
# ahead of the agent inside the container has finished.
service_container() {
    "$RT" ps -q --filter "label=com.docker.compose.project=${PROJECT}" \
                --filter "label=com.docker.compose.service=$1"
}
app="$(service_container app)"
db="$(service_container db)"
for _ in $(seq 30); do
    "$RT" exec "$app" test -f /tmp/am-post-create 2>/dev/null && break
    sleep 1
done
if ! "$RT" exec "$app" test -f /tmp/am-post-create; then
    echo "FAIL: postCreateCommand left no trace in the agent's service" >&2
    exit 1
fi
if "$RT" exec "$db" test -f /tmp/am-post-create 2>/dev/null; then
    echo "FAIL: the hook ran in 'db' too — am execs into the *named* service only" >&2
    exit 1
fi
echo "    postCreateCommand ran in the named service, and only there"

echo "==> am destroy"
in_tmux destroy "$AM destroy $SLUG --force"

count="$(compose_containers)"
if [ "$count" != "0" ]; then
    echo "FAIL: $count containers of $PROJECT survived the destroy" >&2
    exit 1
fi
echo "    the project is gone"

echo
echo "PASS"

#!/usr/bin/env bash
#
# Stand up the local OCI registry the devcontainer integration tests need, and publish the
# purpose-built Features to it.
#
# Why this exists: several parts of `am`'s devcontainer support cannot be tested against the
# public registries at all. Nothing published there declares `dependsOn`, none of them is
# private, and none serves a Feature tarball we control. The tests that would catch a
# regression in those paths need Features that do not exist in the wild — so we publish our
# own.
#
#   ./scripts/test-registry.sh up      # start it and publish the fixtures
#   ./scripts/test-registry.sh down    # stop it
#
# Then:
#
#   cargo test --features integration-registry
#
# Requires a container runtime and the reference CLI (`npm install -g @devcontainers/cli`),
# which is a *test* dependency — `am` itself never runs it.
#
# ── The localhost forwarder ───────────────────────────────────────────────────────────────
#
# The registry must be reachable as `localhost:5000`, not merely reachable. The reference CLI
# speaks plain HTTP only to localhost and TLS to everything else, so publishing to any other
# name fails with a TLS handshake against an HTTP port. Inside a dev container using
# docker-outside-of-docker the registry runs on the *host*, where its published port is not
# this container's localhost — hence the forwarder. On a bare host the registry is already on
# localhost and the forwarder is skipped.
set -euo pipefail

REGISTRY_NAME="am-test-registry"
REGISTRY_PORT="${AM_TEST_REGISTRY_PORT:-5000}"
PRIVATE_NAME="am-test-registry-private"
PRIVATE_PORT="${AM_TEST_PRIVATE_PORT:-5001}"
PRIVATE_USER="amtest"
PRIVATE_PASS="amtest"
FIXTURES="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/tests/fixtures/registry"
STATE="${TMPDIR:-/tmp}/am-test-registry"

runtime() {
    # An explicit choice wins. Machines that have both podman and docker — GitHub's runners, for
    # one — would otherwise pick by an order this script happens to list, which is a variable
    # nobody chose and nobody sees until something fails.
    if [ -n "${AM_TEST_RUNTIME:-}" ]; then
        echo "$AM_TEST_RUNTIME"
        return
    fi
    for candidate in podman docker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            echo "$candidate"
            return
        fi
    done
    echo "error: no container runtime found" >&2
    exit 1
}

reachable() {
    # 200 anonymously, or 401 from the private one — either proves the port is answering.
    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://localhost:$1/v2/" || true)
    [ "$code" = "200" ] || [ "$code" = "401" ]
}

# What the runtime thinks is going on. Printed on every failure path, because a registry that
# will not answer has already cost someone a CI round trip by the time they read this, and
# "exit 1" on its own buys a second one.
diagnose() {
    local rt
    rt="$(runtime)"
    echo "--- container state ---" >&2
    "$rt" ps -a --filter "name=am-test-registry" >&2 || true
    for name in "$REGISTRY_NAME" "$PRIVATE_NAME"; do
        echo "--- ${name} logs (last 20) ---" >&2
        "$rt" logs --tail 20 "$name" >&2 2>&1 || echo "(no such container)" >&2
    done
}

# Wait for a registry to answer, because "not answering yet" and "not reachable from here" look
# identical from the outside and mean opposite things. A freshly started registry — especially
# one whose image was pulled a moment ago — takes a second or two to listen, and treating that
# as "unreachable" sends the caller down the forwarder path on a host that needs no forwarder,
# where it fails with a message about the wrong problem entirely.
wait_reachable() {
    local port="$1" attempts="${2:-30}"
    for _ in $(seq "$attempts"); do
        reachable "$port" && return 0
        sleep 1
    done
    return 1
}

start_forwarder() {
    local port="$1"
    # Only needed when the published port is not already this namespace's localhost — decided
    # after giving the registry a fair chance to start, not before.
    if wait_reachable "$port"; then
        return
    fi
    echo "==> localhost:${port} did not answer; assuming its port is published elsewhere"
    if ! command -v socat >/dev/null 2>&1; then
        echo "error: localhost:${port} never answered, and socat is not installed to forward" >&2
        echo "       to a port published elsewhere. On a plain host the registry should be on" >&2
        echo "       localhost already — so this usually means it failed to start." >&2
        diagnose
        exit 1
    fi
    echo "==> forwarding localhost:${port} to the host's published port"
    # stdout/stderr detached and disowned: otherwise this script never returns, because the
    # shell waits on a child still holding its stdout.
    socat "TCP-LISTEN:${port},fork,reuseaddr" \
          "TCP:host.containers.internal:${port}" >/dev/null 2>&1 &
    echo $! >> "$STATE/forwarders.pid"
    disown 2>/dev/null || true
    for _ in $(seq 20); do
        reachable "$port" && return
        sleep 0.25
    done
    echo "error: registry still unreachable at localhost:${port} after forwarding" >&2
    diagnose
    exit 1
}

running() {
    "$1" ps --filter "name=$2" --format '{{.Names}}' | grep -q .
}

publish() {
    local registry="$1"
    echo "==> publishing fixture Features to ${registry}/amtest"
    if ! devcontainer features publish \
        --registry "$registry" \
        --namespace amtest \
        "$FIXTURES" >"$STATE/publish.log" 2>&1; then
        cat "$STATE/publish.log" >&2
        echo "error: publishing the fixture Features to ${registry} failed" >&2
        exit 1
    fi
}

# The private registry exists to exercise one thing the public ones cannot: am reading a
# credential out of a runtime auth file and sending it. htpasswd comes from the httpd image
# rather than a host package, because bcrypt is not something to hand-roll in a shell script.
start_private() {
    local rt="$1"
    if running "$rt" "$PRIVATE_NAME"; then
        return
    fi
    echo "==> starting ${PRIVATE_NAME} on port ${PRIVATE_PORT} (basic auth)"
    "$rt" run --rm --entrypoint htpasswd docker.io/library/httpd:2 -Bbn "$PRIVATE_USER" "$PRIVATE_PASS" \
        > "$STATE/htpasswd"
    "$rt" run -d --rm -p "${PRIVATE_PORT}:5000" --name "$PRIVATE_NAME" \
        -e REGISTRY_AUTH=htpasswd \
        -e REGISTRY_AUTH_HTPASSWD_REALM="am test registry" \
        -e REGISTRY_AUTH_HTPASSWD_PATH=/etc/htpasswd \
        docker.io/library/registry:2 >/dev/null
    # Copied in rather than bind-mounted: under docker-outside-of-docker the daemon resolves a
    # `-v` source against the *host's* filesystem, where this path does not exist — so it would
    # helpfully create a directory there and the registry would answer every request with 400.
    "$rt" cp "$STATE/htpasswd" "${PRIVATE_NAME}:/etc/htpasswd"
}

up() {
    local rt
    rt="$(runtime)"
    echo "==> using ${rt}"
    mkdir -p "$STATE"

    if ! running "$rt" "$REGISTRY_NAME"; then
        echo "==> starting ${REGISTRY_NAME} on port ${REGISTRY_PORT}"
        "$rt" run -d --rm -p "${REGISTRY_PORT}:5000" --name "$REGISTRY_NAME" docker.io/library/registry:2 >/dev/null
    fi
    start_private "$rt"

    start_forwarder "$REGISTRY_PORT"
    start_forwarder "$PRIVATE_PORT"

    publish "localhost:${REGISTRY_PORT}"

    # Logging in writes the credential to the same auth file am reads, which is the point: the
    # test proves am finds it there, not that it can be handed one.
    echo "==> logging in to localhost:${PRIVATE_PORT}"
    if ! "$rt" login "localhost:${PRIVATE_PORT}" \
        -u "$PRIVATE_USER" -p "$PRIVATE_PASS" >"$STATE/login.log" 2>&1; then
        cat "$STATE/login.log" >&2
        echo "error: could not log in to the private registry" >&2
        exit 1
    fi
    publish "localhost:${PRIVATE_PORT}"

    echo
    echo "Registries ready:"
    echo "  localhost:${REGISTRY_PORT}/amtest/base:1.0.0        no dependencies"
    echo "  localhost:${REGISTRY_PORT}/amtest/needs-base:1.0.0  declares dependsOn"
    echo "  localhost:${PRIVATE_PORT}/amtest/base:1.0.0        the same, behind basic auth"
    echo
    echo "Run the tests that need it:  cargo test --features integration-registry"
    echo "  (add --features integration-cli for the differential dependsOn test)"
}

down() {
    local rt
    rt="$(runtime)"
    if [ -f "$STATE/forwarders.pid" ]; then
        while read -r pid; do
            kill "$pid" 2>/dev/null || true
        done < "$STATE/forwarders.pid"
        rm -f "$STATE/forwarders.pid"
    fi
    "$rt" logout "localhost:${PRIVATE_PORT}" >/dev/null 2>&1 || true
    "$rt" rm -f "$REGISTRY_NAME" "$PRIVATE_NAME" >/dev/null 2>&1 || true
    echo "==> stopped"
}

case "${1:-up}" in
    up) up ;;
    down) down ;;
    *)
        echo "usage: $0 [up|down]" >&2
        exit 1
        ;;
esac

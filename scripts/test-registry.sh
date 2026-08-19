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

# A docker-format config.json holding the private registry's credential, for the reference CLI
# alone. The CLI reads `$DOCKER_CONFIG/config.json` and nothing else — it has no idea podman
# keeps its credentials in `$XDG_RUNTIME_DIR/containers/auth.json`, so under podman the publish
# to the private registry fails with "No basic auth credentials to send" even though `login`
# just succeeded. This is only about letting the *fixtures get published*; `am` is still left to
# find the runtime's own auth file, which is the thing under test.
cli_docker_config() {
    mkdir -p "$STATE/docker-config"
    printf '{"auths":{"localhost:%s":{"auth":"%s"}}}\n' \
        "$PRIVATE_PORT" "$(printf '%s:%s' "$PRIVATE_USER" "$PRIVATE_PASS" | base64 -w0)" \
        > "$STATE/docker-config/config.json"
    echo "$STATE/docker-config"
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

# Check the things the tests assume, from the shell, before any test runs.
#
# Two of the tests depend on properties of this environment rather than of `am`: that the
# private registry actually refuses anonymous callers, and that the credential `login` wrote is
# somewhere `am` looks. When one of those is untrue the test failure names an assertion in Rust
# and says nothing about the registry — so it gets checked here instead, where the message can
# be about what is actually wrong.
verify_contract() {
    local rt="$1" code

    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
        "http://localhost:${REGISTRY_PORT}/v2/amtest/base/manifests/1.0.0" \
        -H 'Accept: application/vnd.oci.image.manifest.v1+json' || true)
    if [ "$code" != "200" ]; then
        echo "error: the published Feature is not readable anonymously (HTTP ${code})" >&2
        diagnose
        exit 1
    fi

    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
        "http://localhost:${PRIVATE_PORT}/v2/" || true)
    if [ "$code" != "401" ]; then
        echo "error: the private registry answered ${code} anonymously, expected 401." >&2
        echo "       Its htpasswd did not take effect, so the test that proves am sends" >&2
        echo "       credentials would pass without am sending any." >&2
        diagnose
        exit 1
    fi

    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
        -u "${PRIVATE_USER}:${PRIVATE_PASS}" "http://localhost:${PRIVATE_PORT}/v2/" || true)
    if [ "$code" != "200" ]; then
        echo "error: the private registry rejected the credentials it was configured with" >&2
        echo "       (HTTP ${code}) — so nothing could authenticate to it, am included." >&2
        diagnose
        exit 1
    fi

    # Where the credential landed, and in what form. `am` reads inline `auth` entries and
    # credential helpers both, but only if it can find the file — and which file `login` writes
    # depends on the runtime and the machine.
    echo "==> credential written by ${rt} login:"
    local found=no
    for f in "${DOCKER_CONFIG:-$HOME/.docker}/config.json" \
             "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/containers/auth.json" \
             "$HOME/.config/containers/auth.json"; do
        [ -f "$f" ] || continue
        python3 - "$f" "localhost:${PRIVATE_PORT}" <<'PYEOF' && found=yes
import json, sys
path, host = sys.argv[1], sys.argv[2]
try:
    doc = json.load(open(path))
except Exception as e:
    print(f"    {path}: unreadable ({e})")
    raise SystemExit(1)
entry = (doc.get("auths") or {}).get(host)
store = doc.get("credsStore")
helpers = (doc.get("credHelpers") or {}).get(host)
if entry is None and not helpers:
    raise SystemExit(1)
how = "inline auth" if (entry or {}).get("auth") else "no inline auth"
print(f"    {path}: entry for {host} present ({how})"
      + (f", credsStore={store}" if store else "")
      + (f", credHelper={helpers}" if helpers else ""))
PYEOF
    done
    if [ "$found" != "yes" ]; then
        echo "error: no auth entry for localhost:${PRIVATE_PORT} in any file am reads." >&2
        echo "       ${rt} login reported success, so it stored the credential somewhere" >&2
        echo "       else — that is the bug, not the test." >&2
        exit 1
    fi
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
    #
    # `--tls-verify=false` is podman-only and not optional: docker treats localhost as an
    # insecure registry implicitly, podman does not, so without it `podman login` speaks TLS to
    # an HTTP port and fails with "server gave HTTP response to HTTPS client". Docker has no
    # such flag, hence the branch rather than an unconditional argument.
    echo "==> logging in to localhost:${PRIVATE_PORT}"
    local login_args=()
    case "$rt" in
        *podman*) login_args+=(--tls-verify=false) ;;
    esac
    if ! "$rt" login "localhost:${PRIVATE_PORT}" "${login_args[@]}" \
        -u "$PRIVATE_USER" -p "$PRIVATE_PASS" >"$STATE/login.log" 2>&1; then
        cat "$STATE/login.log" >&2
        echo "error: could not log in to the private registry" >&2
        exit 1
    fi
    DOCKER_CONFIG="$(cli_docker_config)" publish "localhost:${PRIVATE_PORT}"

    verify_contract "$rt"

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

# Development environment for `am` itself.
#
# Everything here is baked into the image rather than installed by a lifecycle
# hook, because `am` runs its containers with `--rm`: a postCreateCommand would
# re-run on every `am start`, and re-downloading jj on each session start is not
# a reasonable way to begin work.
#
# The build context is the repo root (see "context" in devcontainer.json) so
# requirements-docs.txt stays the single source of truth for the docs toolchain.

FROM mcr.microsoft.com/devcontainers/rust:2-bookworm

# Keep in step with the jj version the project is developed against.
# renovate: datasource=github-releases depName=jj-vcs/jj
ARG JJ_VERSION=0.44.0

# The reference Dev Containers CLI. Pinned, and kept in step with
# .github/workflows/integration.yml: the differential tests assert that am's output matches this
# implementation's, so an unpinned CLI would turn an upstream release into a local failure that
# CI does not reproduce — or the reverse.
# renovate: datasource=npm depName=@devcontainers/cli
ARG DEVCONTAINER_CLI_VERSION=0.89.0

# tmux — `am` drives tmux, and it is needed to exercise a session by hand.
#   The test suite mocks it via AM_TMUX_BIN and does not require it.
# python3-pip — for the docs toolchain below.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      tmux \
      python3-pip \
 && rm -rf /var/lib/apt/lists/*

# jj — the cucumber suite's "a jj repository" step shells out to the real
# binary, so this is a hard requirement for `cargo test`, not a convenience.
# The musl build is used because it is static and has no glibc constraints.
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) jj_arch='x86_64' ;; \
      arm64) jj_arch='aarch64' ;; \
      *) echo "unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/v${JJ_VERSION}/jj-v${JJ_VERSION}-${jj_arch}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 --wildcards '*/jj'; \
    chown root:root /usr/local/bin/jj; \
    jj --version

# The reference CLI, for the differential tests (`cargo test --features integration-cli`).
# `am` itself never runs it — the Node builder was removed — so this is a test dependency in the
# same sense as tempfile.
#
# Installed from the published tarball rather than with `npm install -g`, because npm is not here
# yet: node arrives via the devcontainer Feature, which runs *after* this image is built. The
# package is self-contained (no runtime dependencies, one bundled entry point), so unpacking it
# is a complete install; only `node` itself is resolved later, at run time, from whatever the
# Feature put on PATH.
ARG DEVCONTAINER_CLI_VERSION
RUN set -eux; \
    mkdir -p /opt/devcontainers-cli; \
    curl -fsSL "https://registry.npmjs.org/@devcontainers/cli/-/cli-${DEVCONTAINER_CLI_VERSION}.tgz" \
      | tar -xz -C /opt/devcontainers-cli --strip-components=1; \
    printf '%s\n' \
      '#!/bin/sh' \
      '# node comes from the node Feature, which installs after this image is built, so it is' \
      '# resolved here at run time rather than baked in.' \
      'if ! command -v node >/dev/null 2>&1; then' \
      '    echo "devcontainer: node is not on PATH — the node Feature provides it" >&2' \
      '    exit 127' \
      'fi' \
      'exec node /opt/devcontainers-cli/devcontainer.js "$@"' \
      > /usr/local/bin/devcontainer; \
    chmod 0755 /usr/local/bin/devcontainer

# Docs toolchain, so `mkdocs serve` works without the separate am-docs image.
COPY requirements-docs.txt /tmp/requirements-docs.txt
RUN pip3 install --break-system-packages --no-cache-dir -r /tmp/requirements-docs.txt \
 && rm /tmp/requirements-docs.txt

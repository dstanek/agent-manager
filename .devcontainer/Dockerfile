# Development environment for `am` itself.
#
# Everything here is baked into the image rather than installed by a lifecycle
# hook, because `am` runs its containers with `--rm`: a postCreateCommand would
# re-run on every `am start`, and re-downloading jj on each session start is not
# a reasonable way to begin work.
#
# The build context is the repo root (see "context" in devcontainer.json) so
# requirements-docs.txt stays the single source of truth for the docs toolchain.

FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# Keep in step with the jj version the project is developed against.
ARG JJ_VERSION=0.44.0

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

# Docs toolchain, so `mkdocs serve` works without the separate am-docs image.
COPY requirements-docs.txt /tmp/requirements-docs.txt
RUN pip3 install --break-system-packages --no-cache-dir -r /tmp/requirements-docs.txt \
 && rm /tmp/requirements-docs.txt

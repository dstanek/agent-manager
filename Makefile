DOCKER := $(shell command -v podman 2>/dev/null || command -v docker 2>/dev/null)
ifeq ($(DOCKER),)
$(error "Neither podman nor docker found on PATH")
endif

.PHONY: images build-claude build-claude-minimal build-copilot build-copilot-minimal build-rust-example build-docs-image docs docs-serve

images: build-claude build-claude-minimal build-copilot build-copilot-minimal

build-claude:
	$(DOCKER) build -f dockerfiles/Dockerfile.claude -t am-claude:latest .

build-claude-minimal:
	$(DOCKER) build -f dockerfiles/Dockerfile.claude-minimal -t am-claude-minimal:latest .

build-copilot:
	$(DOCKER) build -f dockerfiles/Dockerfile.copilot -t am-copilot:latest .

build-copilot-minimal:
	$(DOCKER) build -f dockerfiles/Dockerfile.copilot-minimal -t am-copilot-minimal:latest .

build-rust-example: build-claude
	$(DOCKER) build --build-arg BASE_IMAGE=am-claude:latest \
	    -f examples/Dockerfile.rust -t am-rust:latest .

build-docs-image:
	$(DOCKER) build -f dockerfiles/Dockerfile.docs -t am-docs:latest .

docs:
	$(DOCKER) run --rm -v "$(PWD):/docs" am-docs:latest mkdocs build

docs-serve:
	$(DOCKER) run --rm -v "$(PWD):/docs" -p 8000:8000 am-docs:latest mkdocs serve --dev-addr 0.0.0.0:8000

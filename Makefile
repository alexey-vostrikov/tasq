# Сборка и гейты — в контейнере на базе rust:1.99.1-bookworm (см. Dockerfile).
# На хосте rust-тулчейн не нужен: нужен только docker.
#
#   make            — fmt + clippy + deny + test + tokens
#   make build      — release-бинарь в кэше, копия в ~/.local/bin
#   make act        — прогон GitHub Actions локально (nektos/act), тот же образ
#
# Кэш cargo/target живёт вне репозитория, в ~/.cache/tasq, и создаётся от хостового uid.

IMAGE       ?= tasq-builder:1.99.1
CACHE       ?= $(HOME)/.cache/tasq
UID_GID     := $(shell id -u):$(shell id -g)
DOCKER_FILE  = --rm -v $(CURDIR):/src -v $(CACHE)/cargo:/cache/cargo \
               -v $(CACHE)/target:/cache/target -v $(CACHE)/home:/cache/home \
               -u $(UID_GID) -w /src \
               -e CARGO_HOME=/cache/cargo -e CARGO_TARGET_DIR=/cache/target -e HOME=/cache/home
RUN          = docker run $(DOCKER_FILE) $(IMAGE)

.PHONY: all image cache-init fmt clippy deny test build install smoke act clean tokens

all: fmt clippy deny test tokens

image:
	docker build -q -t $(IMAGE) -f Dockerfile .

cache-init: image
	mkdir -p $(CACHE)/cargo $(CACHE)/target $(CACHE)/home
	docker run --rm -v $(CACHE):/cache $(IMAGE) chown -R $(UID_GID) /cache

fmt: cache-init
	$(RUN) cargo fmt --all -- --check

clippy: cache-init
	$(RUN) cargo clippy --all-targets -- -D warnings

# Политика зависимостей: уязвимости, лицензии, источники, дубли, запрещённые крейты.
deny: cache-init
	$(RUN) cargo deny check

test: cache-init
	$(RUN) cargo test --all-targets

build: cache-init
	$(RUN) cargo build --release

install: build
	install -m 0755 $(CACHE)/target/release/tasq $(HOME)/.local/bin/tasq
	$(HOME)/.local/bin/tasq --version

smoke: build
	docker run --rm -v $(CACHE)/target/release:/dist:ro $(IMAGE) sh -c 'set -e; \
	  work=$$(mktemp -d); cd $$work; \
	  /dist/tasq init --key TASQ --name smoke; \
	  /dist/tasq doctor --check; \
	  /dist/tasq doctor --json > doctor.json; \
	  cat doctor.json; grep -q "\"ok\": true" doctor.json'

# Гейт бюджета схем MCP: тот же порог, что в CI.
tokens: build
	$(CACHE)/target/release/tasq tokens --max 6000

act:
	act push -j check

clean:
	docker run --rm -v $(CACHE):/cache $(IMAGE) rm -rf /cache/target /cache/cargo/registry

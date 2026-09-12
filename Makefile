SHELL := /bin/bash
PY    := python3

RUST_VERSION := 1.90
COMPOSE := docker compose
DOCKER := DOCKER_BUILDKIT=1 docker

.PHONY: help ci-local ci-local-deep install-hooks ci-mirror-check \
        build test test-store chart-check check-crate-boundaries lint fmt lock contract-diff up down demo network

help:
	@echo "  make ci-local       run every gate (the pre-push gate, and what CI mirrors)"
	@echo "  make build          compile the workspace"
	@echo "  make test           run the unit tests"
	@echo "  make test-store     run the Postgres store's tests against Postgres"
	@echo "  make chart-check    lint the Helm chart, and check that it refuses bad values"
	@echo "  make check-crate-boundaries  nothing links against another component's store"
	@echo "  make up             bring up Postgres and the replica"
	@echo "  make down           take them down, keeping nothing"
	@echo "  make demo           register this deployment and prove the round trip"
	@echo "  make lint           rustfmt --check and clippy with warnings denied"
	@echo "  make lock           regenerate Cargo.lock"
	@echo "  make install-hooks  point git at hooks/ so push fires ci-local"

# Local green is the completion signal; CI is confirmation.
ci-local: contract-diff ci-mirror-check check-crate-boundaries build test test-store chart-check lint
	@echo
	@echo "ci-local: GREEN"

ci-local-deep: ci-local

ci-mirror-check:
	@$(PY) tools/ci_mirror_check.py --repo-root .

# Storage separation without API separation is decorative: two stores fuse into
# one the moment a second component links against either, because from then on
# the schema is the interface. Nobody argues against the bus; somebody adds a
# dependency for convenience and nothing objects. This objects.
check-crate-boundaries:
	@$(PY) tools/check_crate_boundaries.py --repo-root .

# ADR 005 in meridian-design. Contract-tier changes declare themselves in a
# commit trailer. Reads what changed on disk, so no tool or session root
# avoids it -- which is the whole reason it exists alongside the hook.
contract-diff:
	@$(PY) tools/check_contract_diff.py --self-test
	@$(PY) tools/check_contract_diff.py --repo-root .

build:
	@$(DOCKER) build -f Dockerfile.rust --target check . >/dev/null 2>&1 \
		|| { echo "build FAILED; see it with:" >&2; \
		     echo "  DOCKER_BUILDKIT=1 docker build -f Dockerfile.rust --target check ." >&2; exit 1; }
	@echo "build OK: workspace compiles"

test:
	@$(DOCKER) build -f Dockerfile.rust --target test . >/dev/null 2>&1 \
		|| { echo "test FAILED; see the output with:" >&2; \
		     echo "  DOCKER_BUILDKIT=1 docker build -f Dockerfile.rust --target test --progress=plain ." >&2; exit 1; }
	@echo "test OK: workspace tests pass"

# The store's tests need a database. A build stage has none, and a lightweight
# in-process substitute is exactly what must not exist: a store that behaves
# differently in development is a store nobody has tested.
test-store: network
	@$(COMPOSE) run --rm -T --build tests \
		cargo test --locked -p meridian-reference --test postgres -p meridian-kernel --test postgres \
		>.test-store.log 2>&1 \
		|| { echo "test-store FAILED. The last 40 lines, and the whole of it in .test-store.log:" >&2; \
		     tail -40 .test-store.log >&2; exit 1; }
	@echo "test-store OK: both stores pass against Postgres"

HELM := docker run --rm -v "$(CURDIR)":/w -w /w alpine/helm:3.16.2
CHART_VALUES := --set deployment.id=DEP-check --set key.existingSecret=k --set database.existingSecret=d

# A chart that renders is half the check. The other half is that it refuses:
# a replica with no deployment identifier, no key or no database installs
# happily and then crash-loops, and the operator reads a restart count instead
# of a sentence.
chart-check:
	@$(HELM) lint deploy/chart $(CHART_VALUES) >/dev/null 2>&1 \
		|| { echo "chart-check FAILED: helm lint" >&2; \
		     echo "  docker run --rm -v \"$(CURDIR)\":/w -w /w alpine/helm:3.16.2 lint deploy/chart $(CHART_VALUES)" >&2; exit 1; }
	@$(HELM) template check deploy/chart $(CHART_VALUES) >/dev/null 2>&1 \
		|| { echo "chart-check FAILED: the chart does not render with the three required values" >&2; exit 1; }
	@for missing in deployment.id key.existingSecret database.existingSecret; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) --set $$missing= >/dev/null 2>&1; then \
			echo "chart-check FAILED: the chart rendered with $$missing unset" >&2; exit 1; \
		fi; \
	done
	@echo "chart-check OK: the chart renders, and refuses without each of its three required values"

lint:
	@$(DOCKER) build -f Dockerfile.rust --target lint . >/dev/null 2>&1 \
		|| { echo "lint FAILED; see it with:" >&2; \
		     echo "  DOCKER_BUILDKIT=1 docker build -f Dockerfile.rust --target lint --progress=plain ." >&2; exit 1; }
	@echo "lint OK: formatting and clippy clean"

# Formatting is applied in a container and written back, because the host has no
# toolchain to run it with.
fmt:
	@$(DOCKER) run --rm -v "$(CURDIR)":/w -w /w rust:$(RUST_VERSION)-slim-bookworm \
		sh -c 'rustup component add rustfmt >/dev/null 2>&1; cargo fmt --all'
	@echo "fmt: applied"

lock:
	@$(DOCKER) run --rm -v "$(CURDIR)":/w -w /w rust:$(RUST_VERSION)-slim-bookworm \
		sh -c 'apt-get update >/dev/null && apt-get install -y --no-install-recommends git >/dev/null && cargo generate-lockfile'
	@echo "lock: Cargo.lock regenerated"

install-hooks:
	@git config core.hooksPath hooks
	@echo "hooks installed: git push now runs 'make ci-local' first"

up: network
	@$(COMPOSE) up --build

down:
	@$(COMPOSE) down -v

# The end-to-end check, run rather than described.
#
# It reaches into the platform's compose project, which nothing else here does.
# That is deliberate and confined to this target: core's own compose describes
# no platform, because a second description of one is a second thing to keep
# true. A demo is allowed to know about both.
PLATFORM   ?= ../meridian-platform
ORGANISATION ?= Demo Capital
DEPLOYMENT ?= demo-1

demo: network
	@test -f "$(PLATFORM)/docker-compose.yaml" \
		|| { echo "no platform at $(PLATFORM); set PLATFORM=<path>" >&2; exit 1; }
	@$(COMPOSE) up -d --build postgres
	@echo "1/4  making sure this deployment has a key"
	@mkdir -p .demo
	@$(COMPOSE) run --rm --no-deps -T runtime public-key > .demo/public-key.pem
	@echo "2/4  registering it on the platform"
	@docker compose --project-directory "$(PLATFORM)" -f "$(PLATFORM)/docker-compose.yaml" \
		run --rm -T site python -m django register_deployment \
		--settings platform_site.web.settings \
		--organisation "$(ORGANISATION)" --deployment "$(DEPLOYMENT)" --public-key - \
		< .demo/public-key.pem > .demo/deployment-id \
		|| { echo "registration failed; is the platform up? (make up in $(PLATFORM))" >&2; exit 1; }
	@echo "     deployment $$(cat .demo/deployment-id)"
	@echo "3/6  resolving, missing, pulling, applying"
	@MERIDIAN_DEPLOYMENT_ID="$$(cat .demo/deployment-id)" \
		$(COMPOSE) run --rm --build -T tests cargo test --test end_to_end --locked
	@echo "4/6  taking the platform away"
	@docker compose --project-directory "$(PLATFORM)" -f "$(PLATFORM)/docker-compose.yaml" stop site >/dev/null 2>&1
	@MERIDIAN_DEPLOYMENT_ID="$$(cat .demo/deployment-id)" \
		$(COMPOSE) run --rm -T tests cargo test --test outage --locked \
		|| { docker compose --project-directory "$(PLATFORM)" -f "$(PLATFORM)/docker-compose.yaml" start site >/dev/null 2>&1; exit 1; }
	@echo "5/6  putting it back, and changing nothing else"
	@docker compose --project-directory "$(PLATFORM)" -f "$(PLATFORM)/docker-compose.yaml" start site >/dev/null 2>&1
	@MERIDIAN_DEPLOYMENT_ID="$$(cat .demo/deployment-id)" \
		$(COMPOSE) run --rm -T tests cargo test --test end_to_end --locked
	@echo "6/6  done"

# The network the platform and a deployment's runtime meet on.
#
# Created here rather than by whichever compose project starts first. Compose
# warns on every command when it attaches to a network another project created,
# and `external: true` on both sides with no creator means whoever goes first
# fails. One line, and neither problem exists.
network:
	@docker network inspect meridian >/dev/null 2>&1 || docker network create meridian >/dev/null

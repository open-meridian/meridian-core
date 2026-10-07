SHELL := /bin/bash
PY     := python3

RUST_VERSION := 1.90
COMPOSE := docker compose
DOCKER := DOCKER_BUILDKIT=1 docker

.PHONY: migrate test-broker nats-permissions check-nats-permissions help ci-local ci-local-deep install-hooks ci-mirror-check \
        e2e-first-run-brought e2e-first-run-oidc e2e-cluster e2e-cluster-external \
        test-directory e2e-dashboard-oidc e2e-dashboard-ldap e2e-dashboard-accounts e2e-plugin-page e2e-settings-page harness-check e2e-tickets e2e-activity e2e-access-per-role e2e-archive \
        build test test-store check-image-version chart-check check-crate-boundaries check-one-clock check-test-targets check-local-storage \
        interop e2e-book prompt-attacks lint fmt lock contract-diff up down demo network codegen check-codegen advisories e2e-first-run

help:
	@echo "  make ci-local       run every gate (the pre-push gate, and what CI mirrors)"
	@echo "  make codegen        regenerate the domain bindings from proto/"
	@echo "  make check-codegen  fail if crates/domain/src/v1.rs is stale against proto/"
	@echo "  make build          compile the workspace"
	@echo "  make test           run the unit tests"
	@echo "  make test-store     run the Postgres store's tests against Postgres"
	@echo "  make chart-check    lint the Helm chart, and check that it refuses bad values"
	@echo "  make check-image-version  an image built with a version reports it from every binary"
	@echo "  make check-crate-boundaries  nothing links against another component's store"
	@echo "  make check-one-clock         every component reads the deployment's one clock, and nothing reads the wall clock"
	@echo "  make check-test-targets      every integration test is named by a target that runs it"
	@echo "  make check-local-storage     the development cluster keeps its database across a restart"
	@echo "  make e2e-settings-page  a plugin's Settings pages in a real browser: the entry grid, its most, one screen"
	@echo "  make harness-check  the plugin harness image runs three plugins, end to end"
	@echo "  make e2e-archive    an edge plugin's records archived, restored and returned, a hold refusing on the harness"
	@echo "  make e2e-book       the book of record, written, read and heard through the SDK, then rebuilt"
	@echo "  make up             bring up Postgres and the runtime"
	@echo "  make down           take them down, keeping nothing"
	@echo "  make demo           register this deployment and prove the round trip"
	@echo "  make lint           rustfmt --check and clippy with warnings denied"
	@echo "  make lock           regenerate Cargo.lock"
	@echo "  make install-hooks  point git at hooks/ so push fires ci-local"

# Local green is the completion signal; CI is confirmation.
ci-local: contract-diff ci-mirror-check check-crate-boundaries check-one-clock check-test-targets check-local-storage check-nats-permissions check-codegen advisories build test test-store test-broker test-directory interop e2e-book e2e-dashboard-oidc e2e-dashboard-ldap e2e-dashboard-accounts e2e-plugin-page e2e-settings-page harness-check e2e-tickets e2e-activity e2e-access-per-role e2e-archive e2e-first-run e2e-first-run-brought e2e-first-run-oidc check-image-version chart-check lint
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

# decisions/024: time is the deployment's. Five components once defined a Clock
# each and the bus and the sidecar read the wall clock, so one journal's times
# came from as many sources as there were components and none could be
# replayed. A sixth arrives as one convenient SystemTime::now(); this refuses it.
check-one-clock:
	@$(PY) tools/check_one_clock.py --self-test
	@$(PY) tools/check_one_clock.py --repo-root .

# A tests/ file compiles into its own binary and runs only when a target names
# it. The runtime's wiring test was named by nothing and never ran, while both
# local and CI reported green. This refuses the next one.
check-test-targets:
	@$(PY) tools/check_test_targets.py --self-test --repo-root .
	@$(PY) tools/check_test_targets.py --repo-root .

check-local-storage:
	@for f in deploy/local/*.yaml; do \
		grep -Eq '^[[:space:]]*emptyDir:' "$$f" \
			&& { echo "check-local-storage FAILED: $$f puts a volume on an emptyDir" >&2; \
			     echo "  an emptyDir belongs to the pod; a restart or an eviction empties it" >&2; \
			     echo "  use a PersistentVolumeClaim, as deploy/local/postgres.yaml does" >&2; exit 1; }; \
		true; \
	done
	@grep -q "kind: PersistentVolumeClaim" deploy/local/postgres.yaml \
		|| { echo "check-local-storage FAILED: the development database claims no volume" >&2; exit 1; }
	@grep -q "type: Recreate" deploy/local/postgres.yaml \
		|| { echo "check-local-storage FAILED: the development database rolls rather than recreates" >&2; \
		     echo "  a ReadWriteOnce claim is held by the old pod, so a rolling update never finishes" >&2; exit 1; }
	@echo "check-local-storage OK: the development database outlives its pod"

# ADR 005 in meridian-design. Contract-tier changes declare themselves in a
# commit trailer. Reads what changed on disk, so no tool or session root
# avoids it -- which is the whole reason it exists alongside the hook.
contract-diff:
	@$(PY) tools/check_contract_diff.py --self-test
	@$(PY) tools/check_contract_diff.py --repo-root .

DOMAIN_RS := crates/domain/src/v1.rs
SCRATCH   := .codegen-scratch
# The meridian-schema revision the workspace links, whose protos a domain
# proto may import (bus.proto takes MessageMeta from there).
SCHEMA_REV   := $(shell sed -n 's/^meridian-pb = .*rev = "\([0-9a-f]*\)".*/\1/p' Cargo.toml)
SCHEMA_PROTO := --build-context schema-proto=https://github.com/open-meridian/meridian-schema.git\#$(SCHEMA_REV):proto

# Regenerate in place. The only sanctioned way to change $(DOMAIN_RS).
codegen:
	@rm -rf $(SCRATCH)
	@$(DOCKER) build $(SCHEMA_PROTO) -f Dockerfile.codegen --target export \
		--output type=local,dest=$(SCRATCH) .
	@mv $(SCRATCH)/v1.rs $(DOMAIN_RS) && rm -rf $(SCRATCH)
	@echo "codegen: wrote $(DOMAIN_RS)"

# Generate into a scratch directory and compare. A vendored file that does not
# match a fresh generation is a stale checkout or a hand-edit, and both are the
# same bug: the runtime building against types proto/ does not describe.
check-codegen:
	@rm -rf $(SCRATCH)
	@$(DOCKER) build $(SCHEMA_PROTO) -f Dockerfile.codegen --target export \
		--output type=local,dest=$(SCRATCH) . >/dev/null 2>&1 \
		|| { echo "check-codegen: generation failed; run 'make codegen' to see why" >&2; exit 1; }
	@if diff -q $(DOMAIN_RS) $(SCRATCH)/v1.rs >/dev/null 2>&1; then \
		rm -rf $(SCRATCH); \
		echo "check-codegen OK: $(DOMAIN_RS) matches a fresh generation"; \
	else \
		echo "check-codegen FAILED: $(DOMAIN_RS) is stale or hand-edited" >&2; \
		diff $(DOMAIN_RS) $(SCRATCH)/v1.rs | head -40 >&2; \
		rm -rf $(SCRATCH); \
		echo >&2; \
		echo "Run 'make codegen' and commit the result. Never edit $(DOMAIN_RS) by hand." >&2; \
		exit 1; \
	fi

# Published vulnerabilities in anything the workspace links, from RustSec,
# read fresh each run. Every exception is in deny.toml with its reason.
advisories:
	@$(DOCKER) build -f Dockerfile.rust --target advisories . >/dev/null 2>&1 \
		|| { echo "advisories FAILED: a published vulnerability reaches this workspace. See it with:" >&2; \
		     echo "  DOCKER_BUILDKIT=1 docker build -f Dockerfile.rust --target advisories --progress=plain ." >&2; exit 1; }
	@echo "advisories OK: no published vulnerability reaches the workspace unexplained"

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
		cargo test --locked -p meridian-instrument --test postgres -p meridian-street --test postgres \
			-p meridian-bor --test postgres \
			-p meridian-config --test postgres -p meridian-dashboard --test postgres \
			-p meridian-runtime --test grants --test waiting \
		>.test-store.log 2>&1 \
		|| { echo "test-store FAILED. The last 40 lines, and the whole of it in .test-store.log:" >&2; \
		     tail -40 .test-store.log >&2; exit 1; }
	@echo "test-store OK: the four stores and the dashboard's tables pass against Postgres, the migration grants the serving role what it made, and a component started before its database or its migration waits for it"

# First run, without a cluster: the wizard, the Job, and stand-ins for the two
# things a deployment talks to while it is being set up.
E2E_FIRST_RUN = $(COMPOSE) --profile first-run

# The wizard's two database routes, each proven end to end. `external` points
# at a database somebody already runs; `brought` starts one and makes
# everything in it, which is what a person trying the product does.
E2E_DB_ROUTE ?= external
E2E_ROUTE_SAID = $(if $(filter brought,$(E2E_DB_ROUTE)), on a database it brought and made itself,)

# And which way people sign in: `bundled` is what this deployment does
# itself -- an account it holds -- and `oidc` is the firm's own provider.
E2E_BACKEND ?= local
E2E_BACKEND_SAID = $(if $(filter oidc,$(E2E_BACKEND)), signing people in through the firm's own directory,)

e2e-first-run: network
	@rm -f .e2e-first-run.log
	@$(E2E_FIRST_RUN) down -v --remove-orphans >>.e2e-first-run.log 2>&1 || true
	@set -e; \
	$(E2E_FIRST_RUN) build dashboard-first-run conductor-first-run first-run >>.e2e-first-run.log 2>&1; \
	$(E2E_FIRST_RUN) up -d postgres nats fr-pki fr-kube fr-platform fake-idp >>.e2e-first-run.log 2>&1; \
	$(E2E_FIRST_RUN) run --rm -T fr-database >>.e2e-first-run.log 2>&1; \
	$(E2E_FIRST_RUN) up -d conductor-first-run dashboard-first-run first-run >>.e2e-first-run.log 2>&1; \
	$(E2E_FIRST_RUN) run --rm -T fr-runner > .e2e-first-run.run.log 2>&1 \
		|| { echo "e2e-first-run FAILED. The run:" >&2; tail -40 .e2e-first-run.run.log >&2; \
		     $(E2E_FIRST_RUN) logs --no-color first-run dashboard-first-run conductor-first-run \
		       > .e2e-first-run.services.log 2>&1; \
		     echo "  services in .e2e-first-run.services.log" >&2; \
		     $(E2E_FIRST_RUN) down -v --remove-orphans >>.e2e-first-run.log 2>&1; exit 1; }
	@cat .e2e-first-run.run.log
	@if [ "$(E2E_DB_ROUTE)" = "brought" ]; then \
		$(E2E_FIRST_RUN) exec -T postgres psql -U meridian -d brought -Atc \
		  "select rolname from pg_roles where rolname in ('brought_app','brought_migrate')" \
		  | tee -a .e2e-first-run.log | grep -q brought_app \
		  || { echo "e2e-first-run FAILED: the roles it said it made are not there" >&2; exit 1; }; \
		$(E2E_FIRST_RUN) exec -T postgres psql -U meridian -d brought -Atc \
		  "select has_schema_privilege('brought_app','public','CREATE')" | grep -qx f \
		  || { echo "e2e-first-run FAILED: the serving role may create tables" >&2; exit 1; }; \
		$(E2E_FIRST_RUN) exec -T postgres psql -U meridian -d brought -Atc \
		  "select has_schema_privilege('brought_migrate','public','CREATE')" | grep -qx t \
		  || { echo "e2e-first-run FAILED: the migrating role may not create tables" >&2; exit 1; }; \
		echo "  the roles are real: brought_app may not create tables and brought_migrate may"; \
	fi
	@$(E2E_FIRST_RUN) down -v --remove-orphans >>.e2e-first-run.log 2>&1
	@echo "e2e-first-run OK: an install given nothing, made to serve$(E2E_ROUTE_SAID)$(E2E_BACKEND_SAID)"

# The same run, taking the other route. Its own target rather than a loop, so
# a failure says which route failed without anybody reading a log.
e2e-first-run-brought:
	@$(MAKE) --no-print-directory e2e-first-run E2E_DB_ROUTE=brought

# And the same run again, through the firm's own directory rather than the
# bundled one. Its own target for the same reason, and it exists at all
# because three defects sat in that route undisturbed until 2026-09-23: the
# issuer written where the chart does not read it, the groups claim collected
# and dropped, and the bundle left on. Every other test here chose the bundle.
e2e-first-run-oidc:
	@$(MAKE) --no-print-directory e2e-first-run E2E_BACKEND=oidc

# Path 1 of the five: a real cluster, a real platform, and nobody in it.
#
# Not in ci-local. It wants a Kubernetes and the platform's compose, which a
# runner does not have, and it is the only test here that stands nothing in --
# the others use a Kubernetes that records calls without performing them and a
# platform that implements three of its endpoints in Python. What it reaches
# that they cannot is the wiring from the Job's Secret, through the chart's
# environment, to the permission the conductor writes when it restarts.
E2E_CLUSTER_NAMESPACE ?= meridian-e2e
E2E_EXTERNAL_CONTAINER ?= meridian-e2e-database
E2E_EXTERNAL_PORT ?= 15440
E2E_EXTERNAL_PASSWORD ?= e2e-dev-only
# Its own name: E2E_DB_ROUTE is the compose run's and defaults to `external`
# there, so sharing it would have made `make e2e-cluster` quietly take the
# route nobody asked it for.
E2E_CLUSTER_ROUTE ?= brought
# Set to anything to keep the namespace after a run that passed -- to sign in
# to it from a real browser, say. One that failed is always kept.
E2E_CLUSTER_KEEP ?=
E2E_BROWSER_IMAGE ?= meridian-e2e-browser:local
# Who installs and answers the wizard: this runner, or `meridian up --params`
# from meridian-cli's e2e image (its `make e2e-up` sets these).
E2E_DRIVER ?= runner
E2E_CLI_IMAGE ?= meridian-cli-e2e:local
# How people sign in: `local` (an account the deployment holds) or `ldap`
# (the firm's directory, which e2e-cluster-ldap starts).
E2E_CLUSTER_SIGN_IN ?= local
E2E_LDAP_CONTAINER ?= meridian-e2e-ldap
E2E_LDAP_PORT ?= 15389
E2E_IDP_CONTAINER ?= meridian-e2e-idp
E2E_IDP_PORT ?= 18100
E2E_PLATFORM_FROM_POD ?= http://host.docker.internal:9290
e2e-cluster: network
	@command -v kubectl >/dev/null && kubectl cluster-info >/dev/null 2>&1 \
		|| { echo "e2e-cluster needs a cluster; point KUBECONFIG at one" >&2; exit 1; }
	@test -f "$(PLATFORM)/docker-compose.yaml" \
		|| { echo "no platform at $(PLATFORM); set PLATFORM=<path>" >&2; exit 1; }
	@echo "e2e-cluster: building the image this cluster will run"
	@$(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@# And the browser the password branches are signed in with, in a pod.
	@$(DOCKER) build -q -t $(E2E_BROWSER_IMAGE) -f e2e/cluster/browser.Dockerfile e2e/cluster >/dev/null
	@# A cluster on the daemon that built the images sees them already; one
	@# that is not (k3d, kind) is handed them, or its pods pull tags that exist
	@# nowhere. Empty for Rancher Desktop and Docker Desktop.
	@$(if $(E2E_IMAGE_LOAD),$(E2E_IMAGE_LOAD) $(RUNTIME_IMAGE) $(E2E_BROWSER_IMAGE) $(if $(filter cli,$(E2E_DRIVER)),$(E2E_CLI_IMAGE)) >/dev/null,:)
	# A pod calls the platform host.docker.internal, so the platform has to
	# admit that name and expect it as the audience a deployment signs for.
	# Django refuses an unlisted Host before any view runs, which is a 400 with
	# nothing in the application log -- the same 400 on every endpoint, which
	# is what says it is not the endpoint.
	@MERIDIAN_ALLOWED_HOSTS=localhost,127.0.0.1,site,host.docker.internal \
	 MERIDIAN_EDGE_AUDIENCE=$(E2E_PLATFORM_FROM_POD) \
	 docker compose --project-directory "$(PLATFORM)" -f "$(PLATFORM)/docker-compose.yaml" \
		up -d --build --force-recreate site >.e2e-platform.log 2>&1 \
		|| { echo "e2e-cluster: the platform did not start; the tail of .e2e-platform.log:" >&2; \
		     tail -40 .e2e-platform.log >&2; exit 1; }
	# Its schema, because a compose that starts the site does not apply one and
	# the first command to touch a table is where that shows.
	@docker compose --project-directory "$(PLATFORM)" -f "$(PLATFORM)/docker-compose.yaml" \
		run --rm -T site python -m django migrate --settings platform_site.web.settings \
		>>.e2e-platform.log 2>&1 \
		|| { echo "e2e-cluster: the platform's schema could not be applied; the tail of .e2e-platform.log:" >&2; \
		     tail -40 .e2e-platform.log >&2; exit 1; }
	@kubectl delete namespace $(E2E_CLUSTER_NAMESPACE) --ignore-not-found --wait >/dev/null 2>&1
	@E2E_NAMESPACE=$(E2E_CLUSTER_NAMESPACE) E2E_IMAGE=$(RUNTIME_IMAGE) PLATFORM=$(PLATFORM) \
	 E2E_PLATFORM_FROM_POD=$(E2E_PLATFORM_FROM_POD) E2E_DB_ROUTE=$(E2E_CLUSTER_ROUTE) \
	 E2E_SIGN_IN=$(E2E_CLUSTER_SIGN_IN) E2E_LDAP_SERVER=ldap://host.docker.internal:$(E2E_LDAP_PORT) \
	 E2E_IDP_ISSUER=http://host.docker.internal:$(E2E_IDP_PORT) \
	 E2E_BROWSER_IMAGE=$(E2E_BROWSER_IMAGE) \
	 E2E_DRIVER=$(E2E_DRIVER) E2E_CLI_IMAGE=$(E2E_CLI_IMAGE) \
	 E2E_UPGRADE_FROM=$(E2E_UPGRADE_FROM) E2E_UPGRADE_VERSION=$(E2E_UPGRADE_VERSION) \
	 E2E_EXTERNAL_CONTAINER=$(E2E_EXTERNAL_CONTAINER) E2E_EXTERNAL_PORT=$(E2E_EXTERNAL_PORT) \
	 E2E_EXTERNAL_PASSWORD=$(E2E_EXTERNAL_PASSWORD) \
		$(PY) e2e/cluster/run.py; \
	  held=$$?; \
	  if [ $$held -ne 0 ] || [ -n "$(E2E_CLUSTER_KEEP)" ]; then \
	    echo "e2e-cluster: the namespace is left for reading. Remove it with:" >&2; \
	    echo "  kubectl delete namespace $(E2E_CLUSTER_NAMESPACE)" >&2; \
	  else \
	    kubectl delete namespace $(E2E_CLUSTER_NAMESPACE) --wait >/dev/null 2>&1; \
	  fi; \
	  exit $$held

# Path 2: the same run, against a database somebody already runs.
#
# A container outside the cluster, which is what a firm's own Postgres, a
# managed one from a cloud and one in Docker all look like from in here: a
# host and a port with two roles already on it. The roles are made before any
# of this, by the statements a firm's database administrator would run, which
# is the half of this route the deployment never does.
e2e-cluster-external:
	@docker rm -f $(E2E_EXTERNAL_CONTAINER) >/dev/null 2>&1 || true
	@docker run -d --name $(E2E_EXTERNAL_CONTAINER) \
		-e POSTGRES_PASSWORD=$(E2E_EXTERNAL_PASSWORD) \
		-p $(E2E_EXTERNAL_PORT):5432 postgres:16-alpine >/dev/null
	@# Over TCP, not the socket. The image initialises behind a server that
	@# listens on its socket alone, says ready there, and then restarts; a
	@# check on the socket passes during that first one, and the statements
	@# below land in the gap between the two. The real server is the only one
	@# on TCP.
	@until docker exec $(E2E_EXTERNAL_CONTAINER) pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; do sleep 1; done
	@docker exec $(E2E_EXTERNAL_CONTAINER) psql -h 127.0.0.1 -U postgres -v ON_ERROR_STOP=1 \
		-c "create database meridian" \
		-c "create role meridian_app login password '$(E2E_EXTERNAL_PASSWORD)'" \
		-c "create role meridian_migrate login password '$(E2E_EXTERNAL_PASSWORD)'" \
		>/dev/null
	@docker exec $(E2E_EXTERNAL_CONTAINER) psql -h 127.0.0.1 -U postgres -d meridian -v ON_ERROR_STOP=1 \
		-c "grant usage on schema public to meridian_app, meridian_migrate" \
		-c "grant create on schema public to meridian_migrate" \
		-c "revoke create on schema public from meridian_app, public" >/dev/null
	@$(MAKE) --no-print-directory e2e-cluster E2E_CLUSTER_ROUTE=external; \
	  held=$$?; \
	  docker rm -f $(E2E_EXTERNAL_CONTAINER) >/dev/null 2>&1 || true; \
	  exit $$held

# The same run, signing people in through the firm's LDAP.
#
# Outside the cluster, as path 2's database is, because that is where a
# firm's directory is: a host and a port the deployment was told about. The
# tree is the compose suite's, loaded the way that suite loads it, so both
# mean the same alice and bob. The database is the one the chart brings;
# which database it is and how people sign in are independent, and each has
# its own run so a failure says which it was.
e2e-cluster-ldap:
	@docker rm -f $(E2E_LDAP_CONTAINER) >/dev/null 2>&1 || true
	@docker run -d --name $(E2E_LDAP_CONTAINER) \
		-e LDAP_ROOT=dc=example,dc=org -e LDAP_ADMIN_USERNAME=admin \
		-e LDAP_ADMIN_PASSWORD=ldap-admin-dev-only -e LDAP_SKIP_DEFAULT_TREE=yes \
		-v "$(CURDIR)/e2e/dashboard/ldap":/e2e-ldap:ro \
		-p $(E2E_LDAP_PORT):1389 bitnamilegacy/openldap:2.6 >/dev/null
	@# The image sets itself up on a temporary server, stops it, and starts
	@# the real one; both doors answer during the first, and loading the
	@# overlay into it failed with "Can't contact LDAP server" two runs in
	@# five. So: the real server's own line in the log, then both doors -- the
	@# port a pod will use and the socket the overlay is loaded through.
	@for i in $$(seq 1 60); do docker logs $(E2E_LDAP_CONTAINER) 2>&1 | grep -q "slapd starting" && exit 0; sleep 1; done; \
		echo "e2e-cluster-ldap: the directory never started" >&2; docker rm -f $(E2E_LDAP_CONTAINER) >/dev/null; exit 1
	@docker exec $(E2E_LDAP_CONTAINER) sh -c 'for i in $$(seq 1 60); do ldapsearch $(LDAP_DIR) -b "" -s base >/dev/null 2>&1 && ldapsearch -Q -Y EXTERNAL -H ldapi:/// -b cn=config -s base >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1' \
		|| { echo "e2e-cluster-ldap: the directory did not come up" >&2; docker rm -f $(E2E_LDAP_CONTAINER) >/dev/null; exit 1; }
	@docker exec -i $(E2E_LDAP_CONTAINER) ldapmodify -Q -Y EXTERNAL -H ldapi:/// <e2e/dashboard/ldap/01-memberof.ldif >/dev/null
	@docker exec -i $(E2E_LDAP_CONTAINER) ldapadd $(LDAP_DIR) <e2e/dashboard/ldap/02-tree.ldif >/dev/null
	@$(MAKE) --no-print-directory e2e-cluster E2E_CLUSTER_SIGN_IN=ldap; \
	  held=$$?; \
	  docker rm -f $(E2E_LDAP_CONTAINER) >/dev/null 2>&1 || true; \
	  exit $$held

# The same run, signing people in through the firm's own provider.
#
# The stand-in the compose suite uses, outside the cluster: real RS256, real
# discovery, a real token exchange from the pod. Its issuer is
# host.docker.internal, which is what the pod calls this machine; the runner
# plays the browser and reaches the same name at 127.0.0.1. Both sides
# holding one issuer string is what a real provider gives, and what the
# dashboard checks every token against.
e2e-cluster-oidc:
	@docker rm -f $(E2E_IDP_CONTAINER) >/dev/null 2>&1 || true
	@docker run -d --name $(E2E_IDP_CONTAINER) \
		-e E2E_IDP_ISSUER=http://host.docker.internal:$(E2E_IDP_PORT) -e E2E_IDP_PORT=8100 \
		-v "$(CURDIR)/e2e/dashboard":/e2e:ro \
		-p $(E2E_IDP_PORT):8100 python:3.12-alpine python -u /e2e/fake_idp.py >/dev/null
	@for i in $$(seq 1 30); do curl -sf http://127.0.0.1:$(E2E_IDP_PORT)/.well-known/openid-configuration >/dev/null && exit 0; sleep 1; done; \
		echo "e2e-cluster-oidc: the provider did not come up" >&2; docker rm -f $(E2E_IDP_CONTAINER) >/dev/null; exit 1
	@$(MAKE) --no-print-directory e2e-cluster E2E_CLUSTER_SIGN_IN=oidc; \
	  held=$$?; \
	  docker rm -f $(E2E_IDP_CONTAINER) >/dev/null 2>&1 || true; \
	  exit $$held

# The same run, as an upgrade in place (task kernel/upgrading-a-deployment-in-
# place): the chart published last, installed and set up through its wizard,
# then upgraded to this checkout's chart and image as an administrator does,
# `helm upgrade --reset-then-reuse-values --wait`. It then holds the namespace
# to what an upgrade must leave: every pod on the new image, no container
# restarted, at most three old ReplicaSets a Deployment and no finished Job
# from an earlier revision. Its own target, so a failure says it was the
# upgrade; no browser, terminal or plugin, which the install runs already
# prove and which an upgrade does not change.
#
# The chart comes from where `publish` pushes it, the latest version unless
# E2E_UPGRADE_VERSION names one, and its image from the registry it names: the
# cluster pulls both. Made with the helm on the path, which CI makes Helm 4:
# its --wait is the one that failed.
E2E_UPGRADE_FROM ?=
E2E_UPGRADE_VERSION ?=
E2E_PUBLISHED_CHART ?= oci://ghcr.io/open-meridian/charts/meridian-runtime
e2e-cluster-upgrade:
	@$(MAKE) --no-print-directory e2e-cluster E2E_UPGRADE_FROM=$(E2E_PUBLISHED_CHART)

# Any of the five cluster runs, on a k3d cluster made for it and removed after:
# what CI runs, and what anybody with Docker and k3d can run to reproduce it.
#
#   make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-oidc
#
# The four reach everything outside the cluster -- the platform, a database
# somebody runs, the directory, the provider -- as host.docker.internal.
# Rancher Desktop and Docker Desktop resolve that inside a pod; Linux does not.
# So the cluster is put on a network whose gateway is fixed, the name is
# pointed at that gateway in the nodes and in CoreDNS, and the host's published
# ports answer there. Nothing the runs assert changes.
#
# The run reaches the deployment through the chart's Ingress and the cluster's
# own controller, as a laptop's Rancher Desktop does (spec/live-plugin-
# development, ruling 1): so k3s keeps the Traefik it ships, the cluster's port
# 80 is this machine's, where `<namespace>.localhost` resolves, and the run
# starts once Traefik is serving. Port 80 is a runner's to give; on a laptop
# whose own cluster holds it, run the four there instead.
#
# CoreDNS is restarted once it is made. k3d adds the name after CoreDNS has
# started, and CoreDNS reads that file through a mount that never updates:
# without the restart the name resolves to nothing on a runner, and to the
# desktop's own answer on a laptop, which hides the defect.
#
# Nothing uses the cluster until it is ready. `k3d cluster create --wait`
# returns once the server has started, which is before k3s has made its own
# deployments or its aggregated API answers, and on 2026-09-29 three runs
# failed there before any product code ran: CoreDNS not found by the restart,
# the API server unable to handle the request, and a pre-install hook left
# waiting five minutes. So each step below is waited for in turn, each for at
# most E2E_K3D_READY_SECONDS, with a line saying what and for how long, and on
# giving up the cluster's pods: the API server ready; the default service
# account made, which is the controllers running; k3s's own deployments there
# and available (E2E_K3D_SYSTEM: names, the database's volume, the metrics API
# helm's discovery lists, and the Ingress the run arrives through); and every
# API group discoverable, since a helm install asks for them all. A cluster
# that fails to be created at all is made once more before giving up.
K3D ?= k3d
E2E_K3D_CLUSTER ?= meridian-e2e
E2E_K3D_NETWORK ?= meridian-k3d
E2E_K3D_SUBNET ?= 172.28.0.0/16
E2E_K3D_GATEWAY ?= 172.28.0.1
E2E_K3D_API ?= 127.0.0.1:16443
E2E_K3D_KUBECONFIG ?= $(CURDIR)/.e2e-k3d.kubeconfig
E2E_CLUSTER_TARGET ?= e2e-cluster
# Set to anything to keep the cluster afterwards, for reading.
E2E_K3D_KEEP ?=
E2E_K3D_READY_SECONDS ?= 300
E2E_K3D_SYSTEM ?= coredns local-path-provisioner metrics-server traefik

# `ready <what> <command...>`: the command, every two seconds until it
# succeeds or E2E_K3D_READY_SECONDS have passed.
K3D_READY = ready() { \
	  what="$$1"; shift; start=$$(date +%s); \
	  echo "e2e-cluster-k3d: waiting for $$what, at most $(E2E_K3D_READY_SECONDS)s"; \
	  until "$$@" >/dev/null 2>&1; do \
	    if [ $$(( $$(date +%s) - start )) -ge $(E2E_K3D_READY_SECONDS) ]; then \
	      echo "e2e-cluster-k3d: gave up on $$what after $(E2E_K3D_READY_SECONDS)s. It said:" >&2; \
	      "$$@" >&2 || true; \
	      echo "e2e-cluster-k3d: and the cluster's pods:" >&2; \
	      kubectl get pods -A -o wide >&2 || true; \
	      echo "e2e-cluster-k3d: the cluster is left; remove it with: $(K3D) cluster delete $(E2E_K3D_CLUSTER)" >&2; \
	      return 1; \
	    fi; \
	    sleep 2; \
	  done; \
	  echo "e2e-cluster-k3d:   ready after $$(( $$(date +%s) - start ))s"; \
	}

e2e-cluster-k3d:
	@docker network inspect $(E2E_K3D_NETWORK) >/dev/null 2>&1 \
		|| docker network create --subnet $(E2E_K3D_SUBNET) --gateway $(E2E_K3D_GATEWAY) $(E2E_K3D_NETWORK) >/dev/null
	@$(K3D) cluster delete $(E2E_K3D_CLUSTER) >/dev/null 2>&1 || true
	@echo "e2e-cluster-k3d: a cluster for $(E2E_CLUSTER_TARGET)"
	@: >.e2e-k3d.log; made=; for attempt in 1 2; do \
	  if $(K3D) cluster create $(E2E_K3D_CLUSTER) --network $(E2E_K3D_NETWORK) \
		--host-alias $(E2E_K3D_GATEWAY):host.docker.internal \
		--api-port $(E2E_K3D_API) -p "80:80@loadbalancer" \
		--wait --timeout $(E2E_K3D_READY_SECONDS)s >>.e2e-k3d.log 2>&1; then made=yes; break; fi; \
	  echo "e2e-cluster-k3d: making the cluster failed, attempt $$attempt of 2; the tail of .e2e-k3d.log:" >&2; \
	  tail -20 .e2e-k3d.log >&2; \
	  $(K3D) cluster delete $(E2E_K3D_CLUSTER) >/dev/null 2>&1 || true; \
	done; \
	[ -n "$$made" ] || { echo "e2e-cluster-k3d: the cluster could not be made, twice" >&2; exit 1; }
	@$(K3D) kubeconfig get $(E2E_K3D_CLUSTER) > $(E2E_K3D_KUBECONFIG)
	@$(K3D_READY); export KUBECONFIG=$(E2E_K3D_KUBECONFIG); began=$$(date +%s); \
	ready "the API server (/readyz)" kubectl get --raw /readyz \
	&& ready "the default service account" kubectl -n default get serviceaccount default \
	&& ready "kube-system's deployments to exist: $(E2E_K3D_SYSTEM)" kubectl -n kube-system get deployment $(E2E_K3D_SYSTEM) \
	&& ready "kube-system's deployments to be available" kubectl -n kube-system wait --for=condition=Available --timeout=10s deployment $(E2E_K3D_SYSTEM) \
	&& ready "every API group to be discoverable" kubectl api-resources \
	&& kubectl -n kube-system rollout restart deploy/coredns >/dev/null \
	&& ready "CoreDNS to come back from its restart" kubectl -n kube-system rollout status deploy/coredns --timeout=10s \
	&& echo "e2e-cluster-k3d: the cluster is ready, $$(( $$(date +%s) - began ))s after it was made"
	@KUBECONFIG=$(E2E_K3D_KUBECONFIG) $(MAKE) --no-print-directory $(E2E_CLUSTER_TARGET) \
		E2E_IMAGE_LOAD="$(K3D) image import -c $(E2E_K3D_CLUSTER)"; \
	  held=$$?; \
	  if [ $$held -ne 0 ]; then \
	    echo "e2e-cluster-k3d: what the cluster said, in .e2e-cluster.log" >&2; \
	    { KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl get pods -A -o wide; \
	      KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) get events --sort-by=.lastTimestamp; \
	      for pod in $$(KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) get pods -o name); do \
	        for c in $$(KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) get $$pod \
	            -o jsonpath='{.spec.initContainers[*].name} {.spec.containers[*].name}'); do \
	          echo "== $$pod, $$c"; \
	          KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) logs $$pod -c $$c --tail=80; \
	          echo "== $$pod, $$c before, if it restarted"; \
	          KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) logs $$pod -c $$c --previous --tail=80; \
	        done; \
	      done; } > .e2e-cluster.log 2>&1; \
	  fi; \
	  if [ -n "$(E2E_K3D_KEEP)" ]; then \
	    echo "e2e-cluster-k3d: kept; KUBECONFIG=$(E2E_K3D_KUBECONFIG)" >&2; \
	  else \
	    $(K3D) cluster delete $(E2E_K3D_CLUSTER) >/dev/null 2>&1; \
	    rm -f $(E2E_K3D_KUBECONFIG); \
	  fi; \
	  exit $$held

HELM := docker run --rm -v "$(CURDIR)":/w -w /w alpine/helm:3.16.2
CHART_VALUES := --set deployment.id=DEP-check --set key.existingSecret=k --set key.generate=false --set database.existingSecret=d --set broker.existingSecret=b
PLUGIN_VALUES := --set 'sidecars[0].instanceId=check-1' --set 'sidecars[0].roles={custody}' \
	--set 'sidecars[0].plugin.image=example/plugin:1' --set 'sidecars[0].plugin.existingSecret=vendor'

# A chart that renders is half the check. The other half is that it refuses:
# a component with no deployment identifier, no key or no database installs
# happily and then crash-loops, and the operator reads a restart count instead
# of a sentence.
# The broker's permissions, from the contract. Decisions 010 and 020.
# The broker's permissions, generated by the runtime's own binary.
#
# One implementation of the policy: a bundled broker generates its own
# configuration from the same code at start, so a file here and a chart there
# cannot drift into two policies that must agree.
RUNTIME_IMAGE = meridian-runtime:local
BROKER_CONFIG = $(DOCKER) run --rm -v "$(CURDIR)":/w -w /w $(RUNTIME_IMAGE) meridian-broker-config

runtime-image:
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null

# The committed file is the harness's: it carries the instances this
# repository launches for its own tests. What each may do is the contract's,
# compiled into the image (decisions/020). A deployment's own broker is
# generated in its cluster from its own instances (templates/broker.yaml), and
# never from this.
nats-permissions: runtime-image
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--out /w/deploy/nats/permissions.conf
	@echo "nats-permissions: wrote deploy/nats/permissions.conf"

check-nats-permissions: runtime-image
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--out /w/.permissions.check.conf
	@diff -q deploy/nats/permissions.conf .permissions.check.conf >/dev/null \
		|| { echo "check-nats-permissions FAILED: deploy/nats/permissions.conf has drifted from the contract. Run make nats-permissions" >&2; \
		     rm -f .permissions.check.conf; exit 1; }
	@rm -f .permissions.check.conf
	@echo "nats-permissions OK: deploy/nats/permissions.conf matches the contract"

# The bus across a process boundary, against a real broker. Decision 010.
#
# Two backends on one broker is the whole point: an in-process test proves
# routing, which the memory backend already does, and proves nothing about a
# message leaving a process.
test-broker: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@$(COMPOSE) up -d nats >/dev/null
	@# Restarted rather than left running: a broker holds its permissions from
	@# start, so a config generated a moment ago is not in force until it does.
	@# Skipping this meant a permissions change that had not applied, which is a
	@# gate passing on the wrong configuration.
	@$(COMPOSE) restart nats >/dev/null
	@$(COMPOSE) exec -T nats sh -c 'for i in $$(seq 1 30); do nc -z localhost 4222 && exit 0; sleep 0.5; done; exit 1' \
		|| { echo "test-broker FAILED: the broker did not come back" >&2; exit 1; }
	@$(COMPOSE) run --rm -T --build tests \
		cargo test --locked -p meridian-bus --test nats \
		>.test-broker.log 2>&1 \
		|| { echo "test-broker FAILED. The last 40 lines, and the whole of it in .test-broker.log:" >&2; \
		     tail -40 .test-broker.log >&2; exit 1; }
	@echo "test-broker OK: messages cross a process boundary through the broker"

# Signing in against a real directory. Its own target rather than part of
# `test`, for the reason `test-broker` is: it needs a server standing up, and
# a test that silently skips when one is missing is a gate reporting success
# without doing its job.
# The firm's LDAP, signed in against by the dashboard itself (decisions/018).
# Its own target, so a failure says which branch failed without anybody
# reading a log.
#
# Two phases, because what is being proven is that a change at the directory
# reaches the next sign-in: bob is removed from a group between them.
# As E2E, minus the issuer: this branch has no identity server to point at.
E2E_LDAP := MERIDIAN_DEPLOYMENT_ID=DEP-e2e MERIDIAN_PLATFORM_ADDRESS=http://fake-platform:8000 \
	MERIDIAN_CONFIG_DATABASE_URL=postgres://meridian:meridian@core-postgres:5432/meridian \
	MERIDIAN_DASHBOARD_URL=http://dashboard:8080 \
	MERIDIAN_LDAP_SERVERS=ldap://ldap:1389 \
	MERIDIAN_LDAP_BASE_DN=ou=people,dc=example,dc=org \
	MERIDIAN_LDAP_BIND_DN=cn=admin,dc=example,dc=org \
	MERIDIAN_LDAP_BIND_PASSWORD=ldap-admin-dev-only \
	E2E_CLAIM_CODE=E2E-7KQ2-MX4P \
	$(COMPOSE) --profile e2e
LDAP_DIR = -x -H ldap://localhost:1389 -D cn=admin,dc=example,dc=org -w ldap-admin-dev-only

# The directory image configures its admin against a slapd it has just
# started in the background, and on a busy machine that sometimes exits 255
# with nothing said -- twice in a pre-push on 2026-09-27, straight after the
# OIDC branch, never alone. A container that exits before "slapd starting" is
# replaced by a fresh one rather than failing the gate on the image's race.
e2e-dashboard-ldap: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@: >.e2e-dashboard-ldap.log
	@$(E2E_LDAP) down -v --remove-orphans >>.e2e-dashboard-ldap.log 2>&1 || true
	@set -e; \
	$(E2E_LDAP) build dashboard conductor >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) up -d postgres nats ldap fake-platform >>.e2e-dashboard-ldap.log 2>&1; \
	for i in $$(seq 1 60); do \
	  $(E2E_LDAP) logs ldap 2>&1 | grep -q "slapd starting" && break; \
	  if [ -z "$$($(E2E_LDAP) ps -q --status running ldap)" ]; then \
	    echo "the directory exited in its own setup; a fresh one" >>.e2e-dashboard-ldap.log; \
	    $(E2E_LDAP) rm -fsv ldap >>.e2e-dashboard-ldap.log 2>&1; \
	    $(E2E_LDAP) up -d ldap >>.e2e-dashboard-ldap.log 2>&1; \
	  fi; \
	  sleep 1; \
	done; \
	$(E2E_LDAP) exec -T ldap sh -c 'for i in $$(seq 1 60); do ldapsearch $(LDAP_DIR) -b "" -s base >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1'; \
	$(E2E_LDAP) exec -T ldap ldapmodify -Q -Y EXTERNAL -H ldapi:/// <e2e/dashboard/ldap/01-memberof.ldif >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) exec -T ldap ldapadd $(LDAP_DIR) <e2e/dashboard/ldap/02-tree.ldif >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) run --rm -T conductor meridian-conductor migrate >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) up -d conductor dashboard >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) run --rm -T ldap-runner main; \
	printf 'dn: cn=ldap-group-b,ou=groups,dc=example,dc=org\nchangetype: modify\ndelete: member\nmember: uid=bob,ou=people,dc=example,dc=org\n' \
	  | $(E2E_LDAP) exec -T ldap ldapmodify $(LDAP_DIR) >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) run --rm -T ldap-runner after-removal
	@$(E2E_LDAP) down -v --remove-orphans >>.e2e-dashboard-ldap.log 2>&1
	@echo "e2e-dashboard-ldap OK: the firm's directory signs people in, and a group taken away is gone at the next sign-in"

# Branch one: the firm's own provider, stood in for. These cases borrowed the
# an identity server of ours as a provider, which was always a little false:
# this is the branch where nothing of ours signs anybody in.
E2E_OIDC := MERIDIAN_DEPLOYMENT_ID=DEP-e2e MERIDIAN_PLATFORM_ADDRESS=http://fake-platform:8000 \
	MERIDIAN_CONFIG_DATABASE_URL=postgres://meridian:meridian@core-postgres:5432/meridian \
	MERIDIAN_DASHBOARD_URL=http://dashboard:8080 \
	MERIDIAN_OIDC_ISSUER=http://fake-idp:8100 \
	MERIDIAN_OIDC_CLIENT_ID=meridian-dashboard \
	MERIDIAN_OIDC_CLIENT_SECRET=idp-dev-only-secret \
	E2E_CLAIM_CODE=E2E-7KQ2-MX4P \
	$(COMPOSE) --profile e2e

e2e-dashboard-oidc: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@: >.e2e-dashboard-oidc.log
	@$(E2E_OIDC) down -v --remove-orphans >>.e2e-dashboard-oidc.log 2>&1 || true
	@set -e; \
	$(E2E_OIDC) build dashboard conductor >>.e2e-dashboard-oidc.log 2>&1; \
	$(E2E_OIDC) up -d postgres nats fake-platform fake-idp >>.e2e-dashboard-oidc.log 2>&1; \
	$(E2E_OIDC) run --rm -T conductor meridian-conductor migrate >>.e2e-dashboard-oidc.log 2>&1; \
	$(E2E_OIDC) up -d conductor dashboard >>.e2e-dashboard-oidc.log 2>&1; \
	$(E2E_OIDC) run --rm -T oidc-runner
	@$(E2E_OIDC) down -v --remove-orphans >>.e2e-dashboard-oidc.log 2>&1
	@echo "e2e-dashboard-oidc OK: the firm's own provider signs people in, and a stale or misdirected callback does not"

# Branch three: a firm with no directory at all, so the deployment holds the
# account. The hash below is what first run writes -- Argon2id at this crate's
# defaults, of the password the runner types. `e2e-first-run` asserts the Job
# produces a hash of that shape; this asserts the dashboard turns one into a
# working account. The two halves meet at the format, which is the seam and is
# said out loud rather than left to be discovered.
E2E_ACCOUNT_HASH := $$argon2id$$v=19$$m=19456,t=2,p=1$$bRwFidvdsjyWKVRlZIcW/g$$iHiNbcC7a78/4w3nTa0eDCBs/ZlaVzjoGBGb+bCQi30
E2E_ACCOUNTS := MERIDIAN_DEPLOYMENT_ID=DEP-e2e MERIDIAN_PLATFORM_ADDRESS=http://fake-platform:8000 \
	MERIDIAN_CONFIG_DATABASE_URL=postgres://meridian:meridian@core-postgres:5432/meridian \
	MERIDIAN_DASHBOARD_URL=http://dashboard:8080 \
	MERIDIAN_LOCAL_ACCOUNTS=on \
	MERIDIAN_LOCAL_ACCOUNTS_DATABASE_URL=postgres://meridian:meridian@core-postgres:5432/meridian \
	MERIDIAN_LOCAL_ACCOUNT_NAME=ada \
	MERIDIAN_LOCAL_ACCOUNT_DISPLAY_NAME="Ada Park" \
	MERIDIAN_LOCAL_ACCOUNT_PASSWORD_HASH='$(E2E_ACCOUNT_HASH)' \
	E2E_CLAIM_CODE=E2E-7KQ2-MX4P \
	$(COMPOSE) --profile e2e

# The deployment's plugin registry, which a terminal pushes through the
# dashboard to: the same image the chart runs. Set only for this suite, so the
# plugin-page suite that shares E2E_ACCOUNTS keeps serving none.
E2E_ACCOUNTS_REGISTRY := MERIDIAN_REGISTRY_UPSTREAM=http://e2e-registry:5000 $(E2E_ACCOUNTS)

e2e-dashboard-accounts: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@: >.e2e-dashboard-accounts.log
	@$(E2E_ACCOUNTS) down -v --remove-orphans >>.e2e-dashboard-accounts.log 2>&1 || true
	@set -e; \
	$(E2E_ACCOUNTS) build dashboard conductor >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) up -d postgres nats fake-platform e2e-registry >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T conductor meridian-conductor migrate >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T dashboard meridian-dashboard migrate >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS_REGISTRY) up -d conductor dashboard >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner main; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner retired; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner delegate; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner tickets; \
	$(E2E_ACCOUNTS_REGISTRY) up -d --force-recreate --no-deps dashboard >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner restarted; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner locked
	@$(E2E_ACCOUNTS) down -v --remove-orphans >>.e2e-dashboard-accounts.log 2>&1
	@echo "e2e-dashboard-accounts OK: the account first run made signs somebody in, the terminal sessions from before delegations are served no more, a CLI's delegation outlives a dashboard restart, uploads a plugin and is revoked by a reused refresh token, the admin and its client, a problem reported on a page and through /mcp is worked only at its page with nothing of it reaching the platform, and enough wrong passwords stop it"

# A person reaches a plugin's page (W6.9, decisions/014 and 021), in processes
# of their own: the account branch's dashboard, holding a key made as the
# chart's Job makes it, a sidecar with its front door open, and a stand-in
# plugin in the sidecar's namespace saying what reached it. The stand-in also
# reports the accounts its connection reaches and a sync state, which the
# dashboard lists beside its link action and shows with what to do (W2.8,
# W2.1, W6.4).
#
# The stand-in heartbeats with SnapTrade's figures, which its sidecar carries
# on its report and the dashboard draws as tiles on its Summary under Manage,
# and with nine, which its sidecar refuses naming the bound, reporting it
# alive, not healthy and with no figures (W4.5, W4.8, W6.9).
#
# The stand-in declares a required secret, and a deployment admin sets it in
# the plugin's settings form: its sidecar's report turns healthy with the
# plugin never restarted (W6.11, W4.7). The secret is then looked for where it
# must not be -- every page and report, by the runner; every component's log
# and the configuration store's table, here. Not the broker's log: the
# development broker traces every payload (-DV), the configuration reply that
# carries a secret to its sidecar included, which a deployment's does not.
E2E_SETTING_SECRET := sk-test-not-a-real-key-e2e-5c1d
E2E_PLUGIN_PAGE := MERIDIAN_PLUGIN_FRONT_DOOR='http://sidecar-{instance}:9292' \
	MERIDIAN_FRONT_DOOR_ADDRESS=0.0.0.0:9292 \
	E2E_SETTING_SECRET=$(E2E_SETTING_SECRET) \
	$(E2E_ACCOUNTS)

e2e-plugin-page: network
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "e2e-plugin-page FAILED: the SDK's image did not build" >&2; exit 1; }
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@: >.e2e-plugin-page.log
	@$(E2E_PLUGIN_PAGE) down -v --remove-orphans >>.e2e-plugin-page.log 2>&1 || true
	@set -e; \
	$(E2E_PLUGIN_PAGE) build dashboard conductor sidecar street >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) up -d postgres nats fake-platform >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) run --rm -T plugin-page-keys >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) run --rm -T settings-key >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) run --rm -T conductor meridian-conductor migrate >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) run --rm -T street meridian-street migrate >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) run --rm -T dashboard meridian-dashboard migrate >>.e2e-plugin-page.log 2>&1; \
	printf '%s\n' "INSERT INTO dashboard_local_account (name, display_name, password_hash, created_at_ns) VALUES ('bea', 'Bea Stone', :'hash', 0);" \
		| $(E2E_PLUGIN_PAGE) exec -T postgres psql -v ON_ERROR_STOP=1 -v 'hash=$(E2E_ACCOUNT_HASH)' -U meridian -d meridian >>.e2e-plugin-page.log 2>&1; \
	$(E2E_PLUGIN_PAGE) up -d conductor street dashboard sidecar plugin-page >>.e2e-plugin-page.log 2>&1; \
	status=0; $(E2E_PLUGIN_PAGE) run --rm -T plugin-page-runner || status=$$?; \
	if [ $$status -ne 0 ]; then $(E2E_PLUGIN_PAGE) logs dashboard sidecar plugin-page >>.e2e-plugin-page.log 2>&1; \
		echo "e2e-plugin-page FAILED; the components' logs are in .e2e-plugin-page.log" >&2; \
		$(E2E_PLUGIN_PAGE) down -v --remove-orphans >/dev/null 2>&1; exit 1; fi; \
	$(E2E_PLUGIN_PAGE) logs --no-color conductor dashboard sidecar plugin-page street postgres >.e2e-plugin-page.components.log 2>&1; \
	grep -q "plugin settings changed" .e2e-plugin-page.components.log \
		|| { echo "e2e-plugin-page FAILED: the conductor logged no settings change, so the grep below would prove nothing" >&2; \
		     $(E2E_PLUGIN_PAGE) down -v --remove-orphans >/dev/null 2>&1; exit 1; }; \
	for level in admin write; do \
		grep -E "sent for a person.*at_level.{0,16}$$level" .e2e-plugin-page.components.log >/dev/null \
		|| { echo "e2e-plugin-page FAILED: the sidecar recorded no act sent for a person at $$level (W6.9: the level an act was done under)" >&2; \
		     $(E2E_PLUGIN_PAGE) down -v --remove-orphans >/dev/null 2>&1; exit 1; }; \
	done; \
	if grep -qF "$(E2E_SETTING_SECRET)" .e2e-plugin-page.components.log; then \
		echo "e2e-plugin-page FAILED: the secret setting is in a component's log; see .e2e-plugin-page.components.log" >&2; \
		$(E2E_PLUGIN_PAGE) down -v --remove-orphans >/dev/null 2>&1; exit 1; fi; \
	stored="$$($(E2E_PLUGIN_PAGE) exec -T postgres psql -U meridian -d meridian -Atc \
		"select s::text from config_plugin_setting s where name = 'api_key' and value is null and sealed is not null")"; \
	if [ -z "$$stored" ] || echo "$$stored" | grep -qF "$(E2E_SETTING_SECRET)" \
		|| echo "$$stored" | grep -qi "$$(printf %s '$(E2E_SETTING_SECRET)' | od -An -tx1 | tr -d ' \n')"; then \
		echo "e2e-plugin-page FAILED: the secret is not held sealed in the configuration store" >&2; \
		$(E2E_PLUGIN_PAGE) down -v --remove-orphans >/dev/null 2>&1; exit 1; fi; \
	formed="$$($(E2E_PLUGIN_PAGE) exec -T postgres psql -U meridian -d meridian -Atc \
		"select count(*) from config_plugin_setting_change where name = 'api_key' and action = 1 and secret and value is null and changed_by <> ''")"; \
	if [ "$$formed" -lt 1 ]; then \
		echo "e2e-plugin-page FAILED: the secret set on the form left no record of its own naming who ($$formed)" >&2; \
		$(E2E_PLUGIN_PAGE) down -v --remove-orphans >/dev/null 2>&1; exit 1; fi
	@$(E2E_PLUGIN_PAGE) down -v --remove-orphans >>.e2e-plugin-page.log 2>&1
	@echo "e2e-plugin-page OK: a person opens a plugin on its own host at a level she holds -- Manage, Open or View -- and is told it by its sidecar alone, the session carrying that level and the accounts it reaches; a deployment admin is its admin through All plugins (admin) and configures it no more once that link is withdrawn; under Manage she links the accounts it reaches, to an account and a new one, while the plugin as itself, an unreported account, both names and the read under View are refused; an older plugin's admin pages are read as pages at admin; a command is sent for her only under Open; a person granted admin alone sets its settings, links to an existing account and not a new one, and sees no account's data, and All accounts reaches an account no group lists; a required secret set in its settings form makes it healthy without a restart, sealed at rest and in no page, report or log; each settings change its own record naming who, a secret's only as set, the Settings tab saying who last changed them; the figures it reports on its heartbeat are drawn as tiles on its Summary, and nine are refused naming the bound; and each act sent for a person is logged with its level"

# A plugin's Settings pages in a real browser (the product owner, 2026-10-05,
# of SnapTrade's: "why does this screen force scrolling again and do we assume
# only 4 plan-code links will be needed?"). The dashboard's own pages for a
# SnapTrade-shaped instance, served by its ignored test server
# (crates/dashboard/src/admin/tests/served.rs) on the runtime image, with the kit
# that image carries and a stand-in conductor checking and stamping each
# table as the real one does; headless Chromium (e2e/settings-page/browser.py)
# proves the kit's entry grid upgrades on a table's tab, adds rows past four
# and posts them, takes 200 rows and refuses 201, that the plain table still
# posts without script, and that every Settings page and tab fits one screen
# at 1440x900 and 390x844 by the kit's own check; and the Access editor per
# role (contract v15): a row per plugin role, one line each, paged by the
# kit's om-pager, an entry on a role no longer held flagged, a role's level
# posted and held, and a two-role plugin's Access tab by role, each fitting
# both sizes; then again, the fit alone, as a development deployment draws it. Its own network and no published
# port. SHOTS=<dir> keeps a screenshot of each page and size.
SETTINGS_PAGE_IMAGE := meridian-settings-page:local
SETTINGS_BROWSER_IMAGE := meridian-e2e-settings-browser:local
SETTINGS_NET := meridian-core-settings-page
SHOTS ?=

e2e-settings-page:
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(DOCKER) build -f Dockerfile.rust --target settings-page --build-arg RUNTIME_IMAGE=$(RUNTIME_IMAGE) \
		-t $(SETTINGS_PAGE_IMAGE) . >.e2e-settings-page.log 2>&1 \
		|| { echo "e2e-settings-page FAILED: the server did not build; see .e2e-settings-page.log" >&2; exit 1; }
	@$(DOCKER) build -q -f e2e/settings-page/browser.Dockerfile -t $(SETTINGS_BROWSER_IMAGE) e2e/settings-page >/dev/null
	@docker rm -f settings-page >/dev/null 2>&1 || true
	@docker network rm $(SETTINGS_NET) >/dev/null 2>&1 || true
	@docker network create $(SETTINGS_NET) >/dev/null
	@set -e; status=0; \
	for pass in all fit; do \
		development=0; [ "$$pass" = fit ] && development=1; \
		docker run -d --name settings-page --network $(SETTINGS_NET) -e MERIDIAN_SETTINGS_DEVELOPMENT=$$development \
			$(SETTINGS_PAGE_IMAGE) >/dev/null; \
		session=""; \
		for i in $$(seq 1 60); do \
			session="$$(docker logs settings-page 2>/dev/null | sed -n 's/^SESSION=//p')"; \
			[ -n "$$session" ] && docker logs settings-page 2>/dev/null | grep -q '^serving' && break; \
			session=""; sleep 1; \
		done; \
		if [ -z "$$session" ]; then echo "e2e-settings-page FAILED: the server did not start" >&2; \
			docker logs settings-page >>.e2e-settings-page.log 2>&1; status=1; \
		else \
			docker run --rm --network $(SETTINGS_NET) -e E2E_SESSION="$$session" -e PASS=$$pass -e OUT=/out \
				$(if $(SHOTS),-v $(abspath $(SHOTS)):/out,--tmpfs /out) \
				$(SETTINGS_BROWSER_IMAGE) >>.e2e-settings-page.log 2>&1 || status=1; \
		fi; \
		docker rm -f settings-page >/dev/null 2>&1; \
		[ $$status -eq 0 ] || break; \
	done; \
	docker network rm $(SETTINGS_NET) >/dev/null 2>&1 || true; \
	if [ $$status -ne 0 ]; then grep -E "^FAILED|Error|Traceback" -A3 .e2e-settings-page.log | tail -30 >&2; \
		echo "e2e-settings-page FAILED; the whole run is in .e2e-settings-page.log" >&2; exit 1; fi
	@echo "e2e-settings-page OK: on a table setting's own tab, beside Settings in the plugin's area and the admin portal, the kit's entry grid upgrades, adds rows past four and posts them, each stamped with who; a table takes its most, 200 rows, offers no 201st and refuses 201 posted, naming the most; without script the plain table, held rows and three blank, still posts; and Settings, each of its groups' tabs and each table's tab fit one screen at 1440x900 and 390x844 by the kit's own check, on a development deployment too; and the Access editor gives each plugin role a one-line row, paged, flags an entry on a role no longer held, holds a role's level posted, and it and a two-role plugin's Access tab fit both sizes; and an edge plugin's Summary, its parts as tabs, draws per kind what storage and the archive hold and the bytes each uses of the archive, in all against the bound, an archive withdrawn said so, the hold a window is under and its moves paged by the kit's om-pager, Allow archive and Withdraw answered back on it, and the deployment's Holds tab sets a hold, when each was set in full, every part and dialog fitting both sizes with no cell cut off"

# The plugin harness (deploy/harness/README.md), proven as a plugin uses it:
# its own image, files only, built from this tree beside the runtime image,
# copied out, its plugins written by its own `compose` for three of core's
# stand-in plugins, and driven by its runner and its `store` as a plugin's
# e2e drives them. Its own compose project, its own network and no published
# port, so nothing here collides with the targets above; and it names no
# plugin, so nothing here waits on one. A harness that would break a
# plugin's e2e breaks this first.
#
# First, the image: the runtime image holds no harness, the harness image
# holds its five files and nothing else, and none of them -- nor the plugins
# file written from them -- holds a fixed password or hash.
#
# The run: the plugins register, one of them only once the harness has
# started it again after it failed; a deployment admin, signed in with the
# password drawn for this run, sets one plugin's settings, a secret among
# them, in the dashboard's form, and the plugin holds them; defines an
# account; links one of the two accounts the plugin reports through the
# plugin's own form under Manage, with the page's CSRF token and the digest
# of what it offered taken from the page; the plugin, woken by the link,
# records a statement for it as itself; the street store printed by `store
# street` is the expected file exactly, nothing for the account left
# unlinked; the dashboard counts that one not linked; a second plugin is
# opened at write only once the admin is granted it; and `store book` prints
# the book empty.
HARNESS_IMAGE := meridian-harness:local
HARNESS_FILES := README.md activity.sql book.sql compose.yaml harness.py moves.sql street.sql tickets.sql
HARNESS_SECRET := sk-test-harness-not-a-real-key
HARNESS := MERIDIAN_RUNTIME_IMAGE=$(RUNTIME_IMAGE) \
	MERIDIAN_HARNESS_STAND_IN="$(CURDIR)/e2e/plugin-page" \
	$(COMPOSE) -p meridian-core-harness -f .harness/compose.yaml -f .harness/plugins.yaml -f e2e/harness/stand-in.yaml
HARNESS_RUN := $(HARNESS) run --rm -T runner
HARNESS_STORE := $(HARNESS) run --rm -T store

harness-check:
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@$(PY) e2e/harness/known_passwords.py --self-test
	@# One list of edge roles (decisions/028): the harness gives storage to the
	@# plugins the chart does.
	@$(PY) -c 'import re, sys; chart = re.search(r"define \"meridian-runtime.edgeRoles\" -}}\n(.*)\n", open("deploy/chart/templates/_helpers.tpl").read()).group(1).split(","); harness = list(eval(re.search(r"^EDGE_ROLES = (\(.*\))$$", open("deploy/harness/harness.py").read(), re.M).group(1))); sys.exit(0 if chart == harness else f"harness-check FAILED: the harness gives storage to {harness}, the chart to {chart}")'
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q --target harness -t $(HARNESS_IMAGE) . >/dev/null \
		|| { echo "harness-check FAILED: the harness image did not build" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "harness-check FAILED: the SDK's image did not build" >&2; exit 1; }
	@$(DOCKER) run --rm --entrypoint sh $(RUNTIME_IMAGE) -c 'test ! -e /usr/share/meridian/harness' \
		|| { echo "harness-check FAILED: the runtime image still carries the harness at /usr/share/meridian/harness" >&2; exit 1; }
	@rm -rf .harness && id="$$($(DOCKER) create $(HARNESS_IMAGE) none)" \
		&& $(DOCKER) cp "$$id:/harness" .harness >/dev/null \
		&& $(DOCKER) rm "$$id" >/dev/null \
		|| { echo "harness-check FAILED: the harness image carries nothing at /harness" >&2; exit 1; }
	@held="$$(ls -A .harness | LC_ALL=C sort | tr '\n' ' ')"; [ "$$held" = "$(HARNESS_FILES) " ] \
		|| { echo "harness-check FAILED: the harness image holds $$held; it holds $(HARNESS_FILES) and nothing else" >&2; exit 1; }
	@$(DOCKER) run --rm -i -v "$(CURDIR)/.harness":/harness:ro python:3.12-alpine python /harness/harness.py compose \
		<e2e/harness/plugins.json >.harness/plugins.yaml \
		|| { echo "harness-check FAILED: the harness's compose did not write the plugins from e2e/harness/plugins.json" >&2; exit 1; }
	@! printf '[{"instance": "nats", "image": "x", "roles": []}]' \
		| $(DOCKER) run --rm -i -v "$(CURDIR)/.harness":/harness:ro python:3.12-alpine python /harness/harness.py compose >/dev/null 2>&1 \
		|| { echo "harness-check FAILED: the harness's compose wrote a plugin named as one of its own services" >&2; exit 1; }
	@$(PY) e2e/harness/known_passwords.py .harness/* \
		|| { echo "harness-check FAILED: a fixed password or hash is in the harness's files (above, by line)" >&2; exit 1; }
	@: >.e2e-harness.log
	@$(HARNESS) down -v --remove-orphans >>.e2e-harness.log 2>&1 || true
	@started=$$(date +%s); \
	fail() { echo "harness-check FAILED: $$1; the components' logs are in .e2e-harness.log" >&2; \
		$(HARNESS) logs --no-color >>.e2e-harness.log 2>&1; \
		$(HARNESS) down -v --remove-orphans >/dev/null 2>&1; exit 1; }; \
	$(HARNESS) up -d >>.e2e-harness.log 2>&1 || fail "the harness did not start"; \
	$(HARNESS_RUN) ready || fail "the plugin never registered"; \
	$(HARNESS_RUN) settings api_key=$(HARNESS_SECRET) poll_minutes=15 || fail "its settings were not saved"; \
	$(HARNESS_RUN) page --level admin /settings --until '"missing_required": []' >/dev/null \
		|| fail "the plugin never held its settings"; \
	account="$$($(HARNESS_RUN) account 'Harness Brokerage')" || fail "the account was not defined"; \
	$(HARNESS_RUN) form --level admin --page /admin/accounts --post /admin/accounts/link --from-page offered \
		external_account_id=ext-e2e account_id="$$account" --expect "Linked ext-e2e to $$account" >/dev/null \
		|| fail "the plugin's form did not link the account, with what its page offered taken from the page"; \
	for i in $$(seq 1 60); do \
		$(HARNESS_STORE) street >.harness/street 2>>.e2e-harness.log || fail "store street did not print the street store"; \
		grep -q '^statement|Harness Brokerage|stand-in|[0-9]*|complete|' .harness/street && break; \
		sleep 1; \
	done; \
	diff -u e2e/harness/expected.street .harness/street >&2 \
		|| fail "the street store is not e2e/harness/expected.street"; \
	waiting="$$($(HARNESS_RUN) instruments --expect 4)" || fail "the Instruments page did not list the four records the book cannot use"; \
	$(HARNESS_RUN) instrument --identifier 'symbol (stand-in): HRN' asset_class=equity currency=USD \
		source='the stand-in statement' >/dev/null || fail "the admin did not complete a record at the Instruments page"; \
	$(HARNESS_RUN) instruments --expect 3 >/dev/null || fail "a completed record is still listed as one the book cannot use"; \
	$(HARNESS_RUN) mcp connect --covers deployment_admin >/dev/null || fail "an MCP client was not connected on a delegation for the /mcp resource"; \
	$(HARNESS_RUN) mcp list --expect dashboard__complete_instruments >.harness/tools 2>>.e2e-harness.log \
		|| fail "the delegation does not reach dashboard__complete_instruments"; \
	! grep -q '^custody__' .harness/tools || fail "a delegation covering the deployment admin alone lists a plugin's tools"; \
	$(HARNESS_RUN) mcp call dashboard__complete_instruments '{"completions": [{"instrument_id": "LCL-none", "against_version": 1, "values": [{"currency": "USD"}], "source": "s"}]}' \
		--set 'completions[instrument_id=LCL-none].source=a statement (a=b)' --expect-outcome refused --expect 'completions[0].note' >/dev/null || fail "a completion through /mcp without a note was not refused naming completions[0].note"; \
	completed="$$($(HARNESS_RUN) mcp complete --identifier 'symbol (stand-in): SHRT' asset_class=equity currency=USD \
		source='the stand-in statement' note='Completed by the harness agent from the statement.')" \
		|| fail "the agent did not complete a record through /mcp"; \
	through="$$(printf '%s' "$$completed" | sed -n 's/^mcp complete: \([^ ]*\) completed.*/\1/p')"; \
	$(HARNESS_RUN) instruments --expect 2 >/dev/null || fail "the record completed through /mcp is still listed as one the book cannot use"; \
	$(HARNESS_RUN) mcp call dashboard__read_instrument_history "{\"instrument_id\": \"$$through\"}" \
		--expect '"client_name": "harness agent"' --expect '"person": "local|harness"' >/dev/null \
		|| fail "the record's history does not name the person and the client it was completed through"; \
	$(HARNESS_RUN) mcp calls --expect 4 >/dev/null || fail "Connected clients does not list the agent's calls"; \
	unlinked="$$($(HARNESS_RUN) unlinked --expect 1)" || fail "the dashboard did not count the unlinked account"; \
	$(HARNESS_RUN) ready --instance operations >/dev/null || fail "the second plugin never registered"; \
	$(HARNESS_RUN) page --instance operations --level write / >/dev/null 2>&1 \
		&& fail "the admin opened the second plugin at write before being granted it"; \
	$(HARNESS_RUN) grant --instance operations --level write >/dev/null || fail "the admin was not granted write on the second plugin"; \
	$(HARNESS_RUN) page --instance operations --level write / --until '"level": 2' >/dev/null \
		|| fail "the second plugin was not opened at write once granted"; \
	$(HARNESS_RUN) ready --instance restarted >/dev/null || fail "the plugin that failed first never registered: the harness did not start it again"; \
	restarts="$$($(DOCKER) inspect -f '{{.RestartCount}}' "$$($(HARNESS) ps -q restarted)")"; \
	[ "$${restarts:-0}" -ge 1 ] || fail "the plugin that failed first registered without being restarted ($$restarts), so the check proves nothing"; \
	$(HARNESS_STORE) book >.harness/book 2>>.e2e-harness.log || fail "store book did not print the book"; \
	[ ! -s .harness/book ] || fail "the book holds something nobody wrote: $$(head -3 .harness/book)"; \
	$(HARNESS_STORE) ledger >/dev/null 2>&1 && fail "store printed a store it does not have"; \
	$(HARNESS) exec -T -u 65532 custody python -c 'import os, pathlib; pathlib.Path(os.environ["MERIDIAN_STORAGE_DIR"], "kept").write_text("a raw record")' \
		|| fail "the custody plugin could not write its storage as a user that is not root"; \
	$(HARNESS) up -d --force-recreate --no-deps custody >>.e2e-harness.log 2>&1 || fail "the custody plugin's container was not made again"; \
	kept=; for i in $$(seq 1 30); do \
		kept="$$($(HARNESS) exec -T custody python -c 'import os, pathlib; print(pathlib.Path(os.environ["MERIDIAN_STORAGE_DIR"], "kept").read_text())' 2>/dev/null)" && break; \
		sleep 1; \
	done; \
	[ "$$kept" = "a raw record" ] || fail "the custody plugin's storage did not outlive its container: $$kept"; \
	for inner in operations restarted; do \
		$(HARNESS) exec -T $$inner python -c 'import os, sys; sys.exit("MERIDIAN_STORAGE_DIR" in os.environ or os.path.exists("/var/lib/meridian/storage"))' \
			|| fail "$$inner, holding no edge role, was given storage"; \
	done; \
	$(HARNESS) logs --no-color >>.e2e-harness.log 2>&1; \
	$(HARNESS) down -v --remove-orphans >>.e2e-harness.log 2>&1; \
	echo "harness-check OK in $$(( $$(date +%s) - started ))s: the plugin harness is its own image, files only, and the runtime image carries none of it; no fixed password or hash is in its files; its compose writes three plugins, each beside its sidecar, one started again after failing first ($$restarts restart); its runner signs in with the password drawn for the run, sets a plugin's settings, defines an account and links it through the plugin's own form, taking what the page offered from the page; store street prints as expected, the four records it names listed as ones the book cannot use and one completed at the Instruments page, an MCP client connected on a delegation covering the deployment admin alone lists core's tools and no plugin's, is refused a completion without a note by path, completes a record through dashboard__complete_instruments whose history names the person and the client, and Connected clients lists its calls; nothing for the account left unlinked, which the dashboard counts ($$unlinked); a second plugin is opened at write once the admin is granted it, and store book prints the book empty; the custody plugin writes its storage as a user that is not root and finds it again in a new container, and the plugins holding no edge role have none"

# Tickets inside a deployment (contract v13), end to end on the plugin
# harness: core's stand-in as custody, operations and a plugin holding no
# role, two people beside the admin, every step a person's page, an agent's
# /mcp or the stand-in's page (e2e/tickets/run.py says each). Its own compose
# project, so it never meets harness-check's.
e2e-tickets:
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q --target harness -t $(HARNESS_IMAGE) . >/dev/null \
		|| { echo "e2e-tickets FAILED: the harness image did not build" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "e2e-tickets FAILED: the SDK's image did not build" >&2; exit 1; }
	@rm -rf .harness && id="$$($(DOCKER) create $(HARNESS_IMAGE) none)" \
		&& $(DOCKER) cp "$$id:/harness" .harness >/dev/null && $(DOCKER) rm "$$id" >/dev/null
	@$(DOCKER) run --rm -i -v "$(CURDIR)/.harness":/harness:ro python:3.12-alpine python /harness/harness.py compose \
		<e2e/harness/plugins.json >.harness/plugins.yaml
	@MERIDIAN_RUNTIME_IMAGE=$(RUNTIME_IMAGE) $(PY) e2e/tickets/run.py

# The custodian's activity explains a break (contract v14), end to end on the
# plugin harness: core's stand-in as custody and operations, the custody one
# reporting a reinvestment and a sync status needing sign-in, the operations
# one reading them and recording a break linked to the reinvestment, the book
# moving only when the person confirms (e2e/activity/run.py says each step).
# Its own compose project, so it never meets harness-check's or e2e-tickets'.
e2e-activity:
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q --target harness -t $(HARNESS_IMAGE) . >/dev/null \
		|| { echo "e2e-activity FAILED: the harness image did not build" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "e2e-activity FAILED: the SDK's image did not build" >&2; exit 1; }
	@rm -rf .harness && id="$$($(DOCKER) create $(HARNESS_IMAGE) none)" \
		&& $(DOCKER) cp "$$id:/harness" .harness >/dev/null && $(DOCKER) rm "$$id" >/dev/null
	@$(DOCKER) run --rm -i -v "$(CURDIR)/.harness":/harness:ro python:3.12-alpine python /harness/harness.py compose \
		<e2e/harness/plugins.json >.harness/plugins.yaml
	@MERIDIAN_RUNTIME_IMAGE=$(RUNTIME_IMAGE) $(PY) e2e/activity/run.py

# A person's access to a plugin granted per role (contract v15,
# decisions/033), end to end on the plugin harness: core's stand-in launched
# holding custody and operations, four people each holding a level on one role
# or the other, the sidecar refusing an act of a role the person does not
# write naming the role, admin per role at the link and at a setting serving
# both (e2e/access-per-role/run.py says each step). Its own compose project.
e2e-access-per-role:
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q --target harness -t $(HARNESS_IMAGE) . >/dev/null \
		|| { echo "e2e-access-per-role FAILED: the harness image did not build" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "e2e-access-per-role FAILED: the SDK's image did not build" >&2; exit 1; }
	@rm -rf .harness && id="$$($(DOCKER) create $(HARNESS_IMAGE) none)" \
		&& $(DOCKER) cp "$$id:/harness" .harness >/dev/null && $(DOCKER) rm "$$id" >/dev/null
	@$(DOCKER) run --rm -i -v "$(CURDIR)/.harness":/harness:ro python:3.12-alpine python /harness/harness.py compose \
		<e2e/access-per-role/plugins.json >.harness/plugins.yaml
	@MERIDIAN_RUNTIME_IMAGE=$(RUNTIME_IMAGE) $(PY) e2e/access-per-role/run.py

# An edge plugin's older records move to the archive (contract v16), end to
# end on the plugin harness: core's stand-in as two custody plugins built at
# v16, one given the harness's archive and one none, a deployment admin
# allowing the archive and setting a hold, the plugin archiving a unit past
# its window, a person restoring it and reading it back, its return, and the
# deletions and the window the hold refuses (e2e/archive/run.py says each
# step). Its own compose project.
e2e-archive:
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q --target harness -t $(HARNESS_IMAGE) . >/dev/null \
		|| { echo "e2e-archive FAILED: the harness image did not build" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "e2e-archive FAILED: the SDK's image did not build" >&2; exit 1; }
	@rm -rf .harness && id="$$($(DOCKER) create $(HARNESS_IMAGE) none)" \
		&& $(DOCKER) cp "$$id:/harness" .harness >/dev/null && $(DOCKER) rm "$$id" >/dev/null
	@$(DOCKER) run --rm -i -v "$(CURDIR)/.harness":/harness:ro python:3.12-alpine python /harness/harness.py compose \
		<e2e/archive/plugins.json >.harness/plugins.yaml
	@MERIDIAN_RUNTIME_IMAGE=$(RUNTIME_IMAGE) $(PY) e2e/archive/run.py

test-directory: network
	@# Recreated, with a fresh volume, every time. The image keeps its data in
	@# an anonymous volume and treats what it finds there as set up: a
	@# container whose first start was interrupted before it set the admin
	@# password came back "Using persisted data", refused every bind, and
	@# failed this as "the directory did not come up".
	@$(COMPOSE) --profile e2e up -d --force-recreate --renew-anon-volumes ldap >/dev/null
	@# After its own setup server has stopped and the real one started: the
	@# port answers during setup too, and the overlay loaded then is lost.
	@for i in $$(seq 1 60); do $(COMPOSE) logs ldap 2>&1 | grep -q "slapd starting" && exit 0; sleep 1; done; exit 1
	@$(COMPOSE) exec -T ldap sh -c 'for i in $$(seq 1 60); do ldapsearch $(LDAP_DIR) -b "" -s base >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1' \
		|| { echo "test-directory FAILED: the directory did not come up" >&2; exit 1; }
	@# memberOf is an overlay, not a stored attribute. Without it every person
	@# signs in holding no groups, which is a deployment where nobody can do
	@# anything and nothing says why.
	@$(COMPOSE) exec -T ldap ldapmodify -Q -Y EXTERNAL -H ldapi:/// \
		<e2e/dashboard/ldap/01-memberof.ldif >>.test-directory.log 2>&1 || true
	@$(COMPOSE) exec -T ldap ldapadd $(LDAP_DIR) \
		<e2e/dashboard/ldap/02-tree.ldif >>.test-directory.log 2>&1 || true
	@# And the same over ldaps://, with a certificate signed by a CA made now
	@# and trusted by nothing: an encrypted connection is made and verified.
	@rm -rf .e2e-ldap-tls && mkdir -p .e2e-ldap-tls
	@docker run --rm --entrypoint sh -v "$(CURDIR)/.e2e-ldap-tls":/ca -w /ca alpine/openssl:3.3.2 -c '\
		openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=An untrusted CA" -keyout ca.key -out ca.crt && \
		openssl req -newkey rsa:2048 -nodes -subj "/CN=ldap-tls" -keyout server.key -out server.csr && \
		printf "subjectAltName=DNS:ldap-tls\n" > ext.cnf && \
		openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 1 -extfile ext.cnf -out server.crt && \
		chmod 644 server.key' >>.test-directory.log 2>&1
	@$(COMPOSE) --profile e2e up -d --force-recreate --renew-anon-volumes ldap-tls >/dev/null
	@for i in $$(seq 1 60); do $(COMPOSE) logs ldap-tls 2>&1 | grep -q "slapd starting" && exit 0; sleep 1; done; \
		echo "test-directory FAILED: the ldaps:// directory did not come up" >&2; exit 1
	@$(COMPOSE) run --rm -T --build tests \
		cargo test --locked -p meridian-dashboard --test directory \
		>.test-directory.log 2>&1 \
		|| { echo "test-directory FAILED. The last 40 lines, and the whole of it in .test-directory.log:" >&2; \
		     tail -40 .test-directory.log >&2; exit 1; }
	@echo "test-directory OK: people sign in against a real directory, and the ways that go wrong stay apart"

# Every binary in an image built with a version says that version, as publish
# builds it: the components said `0.1.0`, the crate's, whatever release they
# ran (plans/a-plugin-moves-with-its-deployment). Asked of the finished image,
# as `meridian-<binary> --version`, rather than trusting the build's own check.
# After the e2e targets, whose compose builds leave the cargo cache this one
# rebuilds only the runtime crate from.
IMAGE_VERSION_CHECK := 0.0.0+image-version-check
IMAGE_VERSION_TAG   := meridian-core-version-check:local
check-image-version:
	@$(DOCKER) build --target runtime --build-arg MERIDIAN_VERSION=$(IMAGE_VERSION_CHECK) \
		-t $(IMAGE_VERSION_TAG) . >.check-image-version.log 2>&1 \
		|| { echo "check-image-version FAILED: the image did not build. The last 20 lines, and the whole of it in .check-image-version.log:" >&2; \
		     tail -20 .check-image-version.log >&2; exit 1; }
	@bins="$$(docker run --rm --entrypoint sh $(IMAGE_VERSION_TAG) -c 'ls /usr/local/bin/meridian-*')"; \
	[ -n "$$bins" ] || { echo "check-image-version FAILED: the image holds no binaries, so the check would prove nothing" >&2; docker rmi -f $(IMAGE_VERSION_TAG) >/dev/null; exit 1; }; \
	for bin in $$bins; do \
		said="$$(docker run --rm $(IMAGE_VERSION_TAG) $$bin --version)"; \
		[ "$$said" = "$(IMAGE_VERSION_CHECK)" ] \
			|| { echo "check-image-version FAILED: $$bin says '$$said', built as $(IMAGE_VERSION_CHECK)" >&2; docker rmi -f $(IMAGE_VERSION_TAG) >/dev/null; exit 1; }; \
	done; \
	docker rmi -f $(IMAGE_VERSION_TAG) >/dev/null; \
	echo "check-image-version OK: every binary in an image built as $(IMAGE_VERSION_CHECK) says so"

chart-check:
	# A key written twice is not an error to YAML or to Helm: the second wins
	# and the first vanishes. Removing the bundled identity server left two
	# `firstRun:` blocks, which dropped the Job's broker key and rendered a
	# secretKeyRef with no key -- a chart that lints, renders, and that no
	# cluster will accept. Found by the first install on a real one.
	@dup="$$(grep -E '^[A-Za-z][A-Za-z0-9]*:' deploy/chart/values.yaml | cut -d: -f1 | sort | uniq -d)"; \
	[ -z "$$dup" ] || { echo "chart-check FAILED: values.yaml says these twice, and only the second counts: $$dup" >&2; exit 1; }
	@fresh="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null)" \
		|| { echo "chart-check FAILED: the chart does not render as a fresh install, with only the two values the runbook gives" >&2; exit 1; }; \
	echo "$$fresh" | grep -q "first-run" \
		|| { echo "chart-check FAILED: a fresh install rendered no first run, so the check below would prove nothing" >&2; exit 1; }; \
	empty="$$(echo "$$fresh" | grep -nE '^[[:space:]]+key:[[:space:]]*("")?[[:space:]]*$$')"; \
	[ -z "$$empty" ] || { echo "chart-check FAILED: a fresh install renders a Secret reference with no key, which the API server refuses:" >&2; \
		echo "$$empty" >&2; exit 1; }; \
	written="$$(echo "$$fresh" | grep -A1 'resources: \["secrets"\]' | grep resourceNames | tr -d '[]",' | sed 's/.*resourceNames://')"; \
	[ -n "$$written" ] || { echo "chart-check FAILED: found no Secrets the first-run Job may write, so the check below would prove nothing" >&2; exit 1; }; \
	for secret in $$written; do \
		echo "$$fresh" | awk -v n="$$secret" '/^---/{s=0;m=0} /^kind: Secret$$/{s=1} $$0=="  name: "n{m=1} s&&m{f=1} END{exit !f}' \
			|| { echo "chart-check FAILED: first run may write the Secret $$secret and a fresh install does not make it." >&2; \
			     echo "  The Job holds update and never create (decisions/016), so its apply fails with a 404." >&2; exit 1; }; \
	done
	@# Every broker password the chart makes starts with a letter: the broker
	@# reads a variable's value as a value, and one that begins like a number
	@# stops it starting. Rendered three times, because the passwords are
	@# random and the old template drew a bad one in about three renders of four.
	@for i in 1 2 3; do \
		$(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null \
		| awk '/^kind: Secret/{s=1} /^---/{s=0} s && /-password: /{print $$2}' \
		| while read -r encoded; do \
			first=$$(printf '%s' "$$encoded" | base64 -d | cut -c1); \
			case "$$first" in [A-Za-z]) ;; *) echo "chart-check FAILED: a broker password starts with '$$first', which the broker reads as the start of a number" >&2; exit 1;; esac; \
		done || exit 1; \
	done
	@# One host port in the whole chart, the registry's node proxy, and on
	@# 127.0.0.1 alone: on a node's other interfaces anybody who can reach the
	@# node could pull a firm's plugins, and anything else holding a host port
	@# is a pod reachable from outside the cluster by accident.
	@rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null)"; \
	ports="$$(echo "$$rendered" | grep -c 'hostPort:')"; bound="$$(echo "$$rendered" | grep -c 'hostIP: 127.0.0.1')"; \
	[ "$$ports" = 1 ] && [ "$$bound" = 1 ] \
		|| { echo "chart-check FAILED: $$ports host ports and $$bound bound to 127.0.0.1; the registry's node proxy is the only one, on localhost only" >&2; exit 1; }; \
	echo "$$rendered" | grep -q 'REGISTRY_PROXY_REMOTEURL' \
		|| { echo "chart-check FAILED: the node's registry is not a pull-through proxy, so it would accept pushes" >&2; exit 1; }
	@# No version label on the pod template of what runs an image of its own.
	@# A rollout compares the pod template, so the release's version there
	@# restarts the pod on every upgrade: on 2026-09-29 the database restarted
	@# under the migration Job, the Job failed, and the release never came up
	@# (CI run 36631603926). The resources' own metadata keeps the label.
	@rendered="$$($(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null)"; \
	for component in database registry registry-node; do \
		labels="$$(echo "$$rendered" | awk -v n="check-meridian-runtime-$$component" ' \
			/^---/ { doc = 0; meta = 0; tmpl = 0; lab = 0 } \
			/^metadata:/ { meta = 1; next } \
			meta && /^  name: / { doc = ($$2 == n); meta = 0 } \
			doc && /^  template:/ { tmpl = 1; next } \
			tmpl && /^  [^ ]/ { tmpl = 0 } \
			tmpl && /^      labels:/ { lab = 1; next } \
			lab && /^        [^ ]/ { print; next } \
			{ lab = 0 }')"; \
		echo "$$labels" | grep -qx "        meridian.dev/component: $$component" \
			|| { echo "chart-check FAILED: found no pod template for $$component, so the check below would prove nothing" >&2; exit 1; }; \
		echo "$$labels" | grep -q 'app.kubernetes.io/version' \
			&& { echo "chart-check FAILED: the $$component pod template carries the release's version, so every upgrade restarts it" >&2; \
			     echo "  use meridian-runtime.unversionedLabels there; its image does not move with the release" >&2; exit 1; }; \
	done; true
	@# A sidecar on the broker this chart brings: it has a credential under its
	@# instance, and the broker knows its role. Every other render here passes
	@# broker.existingSecret, so for weeks the bundled broker read a sidecar's
	@# instance and role from a key the values do not have, gave it neither,
	@# and no sidecar could have started on it.
	@rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check \
		--set 'sidecars[0].instanceId=check-1' --set 'sidecars[0].roles={custody,reporting}' 2>/dev/null)"; \
	echo "$$rendered" | grep -q '^  check-1: ' \
		|| { echo "chart-check FAILED: the bundled broker makes no credential for a sidecar's instance" >&2; exit 1; }; \
	echo "$$rendered" | grep -q '"instance_id":"check-1","roles":\["custody","reporting"\]' \
		|| { echo "chart-check FAILED: the bundled broker does not know a sidecar's instance and roles" >&2; exit 1; }
	@# decisions/028. A configured plugin holding an edge role gets a claim of
	@# its own, kept when the plugin is removed, mounted in its plugin's
	@# container alone and named in MERIDIAN_STORAGE_DIR; one holding none gets
	@# nothing; the launcher holds the edge shape and the claim it makes; off,
	@# nothing of it renders; and where the cluster has
	@# ValidatingAdmissionPolicy, every plugin pod is held to it.
	@edge="--set sidecars[0].instanceId=edge-1 --set sidecars[0].roles={custody} --set sidecars[0].plugin.image=x/edge:1 \
		--set sidecars[1].instanceId=inner-1 --set sidecars[1].roles={operations} --set sidecars[1].plugin.image=x/inner:1"; \
	rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check $$edge 2>/dev/null)"; \
	echo "$$rendered" | awk '/^---/{c=0} /^kind: PersistentVolumeClaim$$/{c=1} c&&/^  name: check-meridian-runtime-storage-edge-1$$/{n=1} c&&/helm.sh\/resource-policy: keep/{k=1} END{exit !(n&&k)}' \
		|| { echo "chart-check FAILED: a configured plugin holding an edge role has no claim of its own that the chart keeps" >&2; exit 1; }; \
	echo "$$rendered" | grep -q 'claimName: check-meridian-runtime-storage-edge-1$$' \
		|| { echo "chart-check FAILED: the edge plugin's pod does not mount its own claim" >&2; exit 1; }; \
	[ "$$(echo "$$rendered" | grep -c 'mountPath: /var/lib/meridian/storage$$')" = 1 ] \
		&& echo "$$rendered" | grep -A1 'name: MERIDIAN_STORAGE_DIR$$' | grep -q 'value: /var/lib/meridian/storage$$' \
		|| { echo "chart-check FAILED: the storage is not mounted in the plugin's container alone, at MERIDIAN_STORAGE_DIR" >&2; exit 1; }; \
	echo "$$rendered" | grep -q 'storage-inner-1' \
		&& { echo "chart-check FAILED: a plugin holding no edge role was given storage" >&2; exit 1; }; \
	echo "$$rendered" | grep -q '^  claim.json: ' && echo "$$rendered" | grep -q '^  plugin-storage.json: ' \
		|| { echo "chart-check FAILED: the launcher holds no edge shape or no claim to make" >&2; exit 1; }; \
	echo "$$rendered" | grep -A1 'name: MERIDIAN_LAUNCHER_EDGE_ROLES$$' | grep -q 'value: "ccm,custody,dgm,match,reporting,servicing,settlement"$$' \
		|| { echo "chart-check FAILED: the launcher is not told decisions/028's seven edge roles" >&2; exit 1; }; \
	off="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check $$edge --set pluginStorage.enabled=false 2>/dev/null)"; \
	echo "$$off" | grep -q 'storage-edge-1\|claim.json\|MERIDIAN_STORAGE_DIR\|persistentvolumeclaims' \
		&& { echo "chart-check FAILED: with pluginStorage off, storage still renders" >&2; exit 1; }; \
	held="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check \
		--api-versions admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy --show-only templates/plugin-storage.yaml 2>/dev/null)"; \
	echo "$$held" | grep -q "'check-meridian-runtime-storage-' +" \
		&& echo "$$held" | grep -q '(ccm|custody|dgm|match|reporting|servicing|settlement)' \
		&& echo "$$held" | grep -q 'meridian.dev/component: sidecar' \
		|| { echo "chart-check FAILED: the admission policy on plugin pods does not render with their claim, the edge roles and its selector" >&2; exit 1; }
	@# An archive for edge plugins' older records (contract v16): none by
	@# default; a local one a volume and claim of their own, kept, the
	@# conductor told, the launcher given its shape -- the instance's own
	@# directory at MERIDIAN_ARCHIVE_DIR -- and the admission policy holding
	@# every mount of it to that directory; a bucket the conductor told whether
	@# it locks; and the values that cannot stand together refused.
	@base="--set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check"; \
	none="$$($(HELM) template check deploy/chart $$base 2>/dev/null)"; \
	echo "$$none" | grep -q 'MERIDIAN_ARCHIVE\|archive.json\|plugin-archive' \
		&& { echo "chart-check FAILED: with no pluginArchive, an archive still renders" >&2; exit 1; }; \
	local="$$($(HELM) template check deploy/chart $$base --set pluginArchive.path=/srv/archive \
		--api-versions admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy 2>/dev/null)"; \
	echo "$$local" | awk '/^---/{c=0} /^kind: PersistentVolumeClaim$$/{c=1} c&&/^  name: check-meridian-runtime-archive$$/{n=1} c&&/helm.sh\/resource-policy: keep/{k=1} END{exit !(n&&k)}' \
		|| { echo "chart-check FAILED: a local archive is not a claim of its own, kept on uninstall" >&2; exit 1; }; \
	echo "$$local" | grep -A1 'name: MERIDIAN_ARCHIVE$$' | grep -q 'value: "path"' \
		|| { echo "chart-check FAILED: the conductor is not told the deployment keeps a local archive" >&2; exit 1; }; \
	echo "$$local" | grep '^  archive.json: ' | grep -q 'MERIDIAN_ARCHIVE_DIR.*subPath.*__INSTANCE__.*check-meridian-runtime-archive' \
		|| { echo "chart-check FAILED: the launcher's archive shape is not the instance's own directory at MERIDIAN_ARCHIVE_DIR" >&2; exit 1; }; \
	echo "$$local" | grep -q 'm.subPath == object.metadata.labels' \
		|| { echo "chart-check FAILED: the admission policy does not hold an archive's mount to its instance's directory" >&2; exit 1; }; \
	bucket="$$($(HELM) template check deploy/chart $$base --set pluginArchive.bucket=s3://firm/meridian \
		--set pluginArchive.serviceAccount=archive-writer --set pluginArchive.objectLock=true 2>/dev/null)"; \
	echo "$$bucket" | grep -A1 'name: MERIDIAN_ARCHIVE_LOCKS$$' | grep -q 'value: "true"' \
		&& echo "$$bucket" | grep '^  archive.json: ' | grep -q 'MERIDIAN_ARCHIVE_BUCKET.*s3://firm/meridian/__INSTANCE__.*archive-writer' \
		|| { echo "chart-check FAILED: a bucket archive does not render its prefix, its account and whether it locks" >&2; exit 1; }; \
	for refused in "pluginArchive.bucket=s3://x" "pluginArchive.objectLock=true" \
		"pluginArchive.path=/a,pluginArchive.existingClaim=c" "pluginArchive.path=/a,pluginArchive.bucket=s3://x,pluginArchive.serviceAccount=s"; do \
		$(HELM) template check deploy/chart $$base --set "$$refused" >/dev/null 2>&1 \
			&& { echo "chart-check FAILED: pluginArchive $$refused rendered, and cannot stand" >&2; exit 1; }; \
	done; true
	@$(HELM) lint deploy/chart $(CHART_VALUES) >/dev/null 2>&1 \
		|| { echo "chart-check FAILED: helm lint" >&2; \
		     echo "  docker run --rm -v \"$(CURDIR)\":/w -w /w alpine/helm:3.16.2 lint deploy/chart $(CHART_VALUES)" >&2; exit 1; }
	@$(HELM) template check deploy/chart $(CHART_VALUES) >/dev/null 2>&1 \
		|| { echo "chart-check FAILED: the chart does not render with the three required values" >&2; exit 1; }
	@$(HELM) template check deploy/chart --set deployment.id=D --set key.generate=false >/dev/null 2>&1 \
		&& { echo "chart-check FAILED: a deployment with no key and no way to make one rendered anyway" >&2; exit 1; }; true
	@for missing in deployment.id; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) --set $$missing= >/dev/null 2>&1; then \
			echo "chart-check FAILED: the chart rendered with $$missing unset" >&2; exit 1; \
		fi; \
	done
	@unset="$$($(HELM) template check deploy/chart $(CHART_VALUES) --set database.existingSecret= 2>/dev/null)" \
		|| { echo "chart-check FAILED: the chart does not render without a database secret, which is how a fresh install starts" >&2; exit 1; }; \
	echo "$$unset" | grep -q "name: check-meridian-runtime-database" \
		|| { echo "chart-check FAILED: without a database secret the chart makes none for the wizard to fill" >&2; exit 1; }; \
	echo "$$unset" | awk '/name: check-meridian-runtime-database$$/{f=1} f&&/^data:/{print "has data"} /^---/{f=0}' | grep -q . \
		&& { echo "chart-check FAILED: the database secret the chart makes is not empty" >&2; exit 1; }; true
	@rendered="$$($(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null)"; \
	for store in meridian-conductor meridian-street meridian-bor meridian-instrument meridian-dashboard; do \
		echo "$$rendered" | grep -q "\"$$store\", \"migrate\"" \
			|| { echo "chart-check FAILED: the chart renders no migration for $$store" >&2; \
			     echo "  each store verifies its schema and refuses to serve without one" >&2; exit 1; }; \
	done
	@$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| grep -q "runAsUser" \
		&& { echo "chart-check FAILED: the chart pins a uid by default" >&2; \
		     echo "  OpenShift assigns each namespace its own range and refuses a pod asking outside it" >&2; exit 1; } \
		|| true
	@$(HELM) template check deploy/chart --set deployment.id=DEP-check \
		--set key.generate=true --set database.existingSecret=d --set broker.existingSecret=b 2>/dev/null \
		| grep -q "PersistentVolumeClaim" \
		|| { echo "chart-check FAILED: key.generate renders no volume for the key" >&2; exit 1; }
	@if $(HELM) template check deploy/chart $(CHART_VALUES) --set key.generate=true 2>/dev/null | grep -q "PersistentVolumeClaim"; then \
		echo "chart-check FAILED: a deployment that was given a key made a second one anyway" >&2; \
		echo "  generating is the default, and a named secret is what an administrator chose" >&2; exit 1; \
	fi
	@if ! $(HELM) template check deploy/chart --set deployment.id=DEP-check \
		--set database.existingSecret=d --set broker.existingSecret=b >/dev/null 2>&1; then \
		echo "chart-check FAILED: an install that named only its deployment did not render" >&2; \
		echo "  a first install supplies what the platform gave it and nothing else" >&2; exit 1; \
	fi
	@for component in street instrument conductor; do \
		$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
			| grep -q "meridian-$$component\"\]" \
			|| { echo "chart-check FAILED: nothing starts meridian-$$component" >&2; exit 1; }; \
	done
	@rendered="$$($(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null)"; \
	for component in street instrument conductor; do \
		count="$$(echo "$$rendered" | awk '/^kind: Deployment$$/{d=1} d&&/^  name: /{print $$2; d=0}' \
			| grep -c "^check-meridian-runtime-$$component$$")"; \
		[ "$$count" = "1" ] \
			|| { echo "chart-check FAILED: $$component is not a Deployment of its own ($$count found)" >&2; \
			     echo "  one workload means none can be upgraded without the others" >&2; exit 1; }; \
	done
	@# decisions/011. The key authenticates as the whole deployment, so exactly
	@# one component may mount it. A second holder is a second thing that can
	@# speak for the customer, and the last one appeared by accident.
	@$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| awk '/^kind: Deployment/{c=""} /meridian.dev\/component:/{c=$$2} /key.pem/{print c}' \
		| sort -u | grep -qx "conductor" \
		|| { echo "chart-check FAILED: the key is mounted by something that is not the conductor" >&2; exit 1; }
	@test "$$($(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| awk '/^kind: Deployment/{c=""} /meridian.dev\/component:/{c=$$2} /key.pem/{print c}' \
		| sort -u | wc -l | tr -d ' ')" = "1" \
		|| { echo "chart-check FAILED: more than one component mounts the deployment key" >&2; exit 1; }
	@# Requirement 35 as amended 2026-09-28. The key secret plugin settings
	@# are sealed with opens every one of them, so the conductor, which
	@# delivers them, is the one workload that mounts it. The chart makes its
	@# Secret empty and keeps it, and a Job fills it once: a key drawn by a
	@# template would sit in the release's own record.
	@rendered="$$($(HELM) template check deploy/chart $(CHART_VALUES) $(PLUGIN_VALUES) 2>/dev/null)"; \
	holders="$$(echo "$$rendered" | awk '/^---/{n=""} /^  name: / && n==""{n=$$2} /secretName: check-meridian-runtime-settings-key$$/{print n}' | sort -u)"; \
	[ "$$holders" = "check-meridian-runtime-conductor" ] \
		|| { echo "chart-check FAILED: the settings key is mounted by: $$holders; only the conductor mounts it" >&2; exit 1; }; \
	echo "$$rendered" | awk '/^---/{f=0} /^kind: Secret$$/{s=1} /^---/{s=0} s&&/^  name: check-meridian-runtime-settings-key$$/{f=1} f&&/^data:/{print "has data"}' | grep -q . \
		&& { echo "chart-check FAILED: a fresh install renders a settings key; the Job makes it, in the cluster" >&2; exit 1; }; \
	echo "$$rendered" | grep -q '"meridian-conductor", "settings-key"' \
		|| { echo "chart-check FAILED: nothing makes the settings key" >&2; exit 1; }; \
	echo "$$rendered" | awk '/^---/{s=0;f=0} /^kind: Secret$$/{s=1} s&&/^  name: check-meridian-runtime-settings-key$$/{f=1} f&&/helm.sh\/resource-policy: keep/{print; exit}' | grep -q . \
		|| { echo "chart-check FAILED: the settings key's Secret is not kept when the release goes" >&2; exit 1; }
	@$(HELM) template check deploy/chart $(CHART_VALUES) \
		--set 'sidecars[0].instanceId=check-1' --set 'sidecars[0].roles={custody}' 2>/dev/null \
		| grep -q "sidecar-check-1" \
		|| { echo "chart-check FAILED: a configured plugin gets no sidecar" >&2; exit 1; }
	@# A plugin joins its sidecar's pod, and the seam between the two
	@# containers is the boundary: the plugin gets the sidecar's address and
	@# nothing that would let it speak on the bus as itself. Each check reads
	@# only the plugin container's block, so a credential correctly held by the
	@# sidecar beside it cannot satisfy or fail it.
	@$(HELM) template check deploy/chart $(CHART_VALUES) $(PLUGIN_VALUES) --show-only templates/sidecar.yaml 2>/dev/null \
		| grep -q "^        - name: plugin$$" \
		|| { echo "chart-check FAILED: a sidecar given a plugin renders no plugin container" >&2; exit 1; }
	@for forbidden in MERIDIAN_BROKER_URL MERIDIAN_PLUGIN_ROLES MERIDIAN_PLUGIN_TAGS "name: grants"; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) $(PLUGIN_VALUES) --show-only templates/sidecar.yaml 2>/dev/null \
			| awk '/^        - name: plugin$$/{p=1;next} p&&/^        - name: /{p=0} p&&/^      [a-z]/{p=0} p' \
			| grep -q "$$forbidden"; then \
			echo "chart-check FAILED: the plugin container is given $$forbidden" >&2; \
			echo "  the sidecar holds that so the plugin does not; containers share a network, not a filesystem" >&2; exit 1; \
		fi; \
	done
	@$(HELM) template check deploy/chart $(CHART_VALUES) $(PLUGIN_VALUES) --show-only templates/sidecar.yaml 2>/dev/null \
		| awk '/^        - name: plugin$$/{p=1;next} p&&/^        - name: /{p=0} p&&/^      [a-z]/{p=0} p' \
		| grep -q "MERIDIAN_SIDECAR_ADDRESS" \
		|| { echo "chart-check FAILED: the plugin container is not told where its sidecar is" >&2; exit 1; }
	@for secret in b k; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) $(PLUGIN_VALUES) \
			--set "sidecars[0].plugin.existingSecret=$$secret" >/dev/null 2>&1; then \
			echo "chart-check FAILED: a plugin was allowed to read secret $$secret" >&2; \
			echo "  the broker's and the key's secrets must never reach a plugin through envFrom" >&2; exit 1; \
		fi; \
	done
	@$(HELM) template check deploy/chart $(CHART_VALUES) \
		--set 'sidecars[0].instanceId=check-1' --set 'sidecars[0].roles={custody}' \
		--show-only templates/sidecar.yaml 2>/dev/null | grep -q "^        - name: plugin$$" \
		&& { echo "chart-check FAILED: a sidecar with no plugin rendered a plugin container" >&2; exit 1; } || true
	@bundled="$$($(HELM) template check deploy/chart --set deployment.id=D --set key.existingSecret=k 2>/dev/null)"; \
	echo "$$bundled" | grep -q 'meridian.dev/component: broker' \
		|| { echo "chart-check FAILED: with no broker secret the chart brings no broker, so an install still needs one written by hand" >&2; exit 1; }; \
	echo "$$bundled" | grep -q 'meridian-broker-config' \
		|| { echo "chart-check FAILED: the bundled broker does not generate its permissions from the contract" >&2; exit 1; }; \
	$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null | grep -q 'meridian.dev/component: broker' \
		&& { echo "chart-check FAILED: a deployment with its own broker got one from the chart as well" >&2; exit 1; }; true
	@$(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url=https://meridian.example 2>/dev/null \
		| grep -q '"meridian-dashboard"' \
		|| { echo "chart-check FAILED: the dashboard does not render when enabled" >&2; exit 1; }
	@# An address is what a directory needs, and a deployment nobody has set up
	@# has neither: the wizard asks for both. What must not render is a second
	@# dashboard, because one holds the wizard's single session.
	@$(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url= 2>/dev/null | grep -q '"meridian-dashboard"' \
		|| { echo "chart-check FAILED: a deployment with no address yet renders no dashboard, so nothing serves its wizard" >&2; exit 1; }
	@for refused in "dashboard.replicaCount=2"; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
			--set dashboard.url=https://meridian.example --set $$refused >/dev/null 2>&1; then \
			echo "chart-check FAILED: the dashboard rendered with $$refused" >&2; exit 1; \
		fi; \
	done
	@# The client id the dashboard signs people in with is optional, and that
	@# is what makes first run possible: it does not exist until somebody has
	@# been through the wizard, and a required key would leave the dashboard
	@# unable to start and so unable to serve the wizard that fills it.
	@$(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url=https://meridian.example 2>/dev/null \
		| awk '/name: MERIDIAN_OIDC_CLIENT_ID/{f=1} f&&/optional: true/{print "optional"; exit}' | grep -q optional \
		|| { echo "chart-check FAILED: the dashboard cannot start without a client id, and the wizard that makes one is what it would be serving" >&2; exit 1; }
	@# And the firm's LDAP, on the branch where the dashboard binds it itself.
	@$(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url=https://meridian.example 2>/dev/null \
		| awk '/name: MERIDIAN_LDAP_SERVERS/{f=1} f&&/optional: true/{print "optional"; exit}' | grep -q optional \
		|| { echo "chart-check FAILED: the dashboard cannot start without a directory, and first run is what configures one" >&2; exit 1; }
	@role="$$($(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url=https://meridian.example 2>/dev/null \
		| awk '/^kind: Role$$/{r=1} r; /^---/{r=0}' \
		| sed -n '/name: check-meridian-runtime-first-run/,/^---/p')"; \
	echo "$$role" | grep -qE 'verbs:.*(create|\*)' \
		&& { echo "chart-check FAILED: first run may create a resource. RBAC cannot narrow create to a name, so a Job that may create Secrets may create any" >&2; exit 1; }; \
	echo "$$role" | grep -q 'resources: \["rolebindings"\]' \
		|| { echo "chart-check FAILED: first run cannot delete its own binding, so it never gives up its rights" >&2; exit 1; }; \
	echo "$$role" | grep -c 'resourceNames:' | grep -qv '^0$$' \
		|| { echo "chart-check FAILED: a first-run rule names no resource" >&2; exit 1; }; true
	@# The address is the wizard's to learn, so the chart renders without one
	@# and the dashboard reads what first run wrote.
	@without="$$($(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url= 2>/dev/null)"; \
	echo "$$without" | grep -q 'key: issuer' \
		|| { echo "chart-check FAILED: the dashboard does not read the issuer first run wrote" >&2; exit 1; }; \
	echo "$$without" | grep -q 'key: dashboard-url' \
		|| { echo "chart-check FAILED: the dashboard does not read the address first run wrote" >&2; exit 1; }; true
	@# The front door (decisions/014, 021). Each sidecar's HTTP port admits the
	@# dashboard's pods and nothing else, the dashboard's private key is
	@# mounted by the dashboard alone, and the Job that makes it may create
	@# nothing and gives its rights up.
	@rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check \
		--set 'sidecars[0].instanceId=check-1' 2>/dev/null)"; \
	doc() { echo "$$rendered" | awk -v k="kind: $$1" -v n="  name: $$2" \
		'function f(){ if (a && b) printf "%s", d; d=""; a=0; b=0 } /^---/{f(); next} {d=d $$0 "\n"} $$0==k{a=1} $$0==n{b=1} END{f()}'; }; \
	policy="$$(doc NetworkPolicy check-meridian-runtime-sidecars)"; \
	[ -n "$$policy" ] || { echo "chart-check FAILED: a sidecar's front door has no NetworkPolicy, so any pod can reach it" >&2; exit 1; }; \
	[ "$$(echo "$$policy" | grep -c -- '- podSelector:')" = 1 ] && echo "$$policy" | grep -q 'meridian.dev/component: dashboard' \
		&& ! echo "$$policy" | grep -q 'namespaceSelector\|ipBlock' \
		|| { echo "chart-check FAILED: a sidecar's front door admits something besides the dashboard's pods:" >&2; echo "$$policy" >&2; exit 1; }; \
	role="$$(doc Role check-meridian-runtime-dashboard-key)"; \
	[ -n "$$role" ] || { echo "chart-check FAILED: found no Role for the dashboard's key Job, so the checks below would prove nothing" >&2; exit 1; }; \
	echo "$$role" | grep -q '"create"' \
		&& { echo "chart-check FAILED: the dashboard's key Job may create a resource; RBAC cannot narrow create to a name" >&2; exit 1; }; \
	[ "$$(echo "$$role" | grep -c 'resourceNames:')" = 3 ] \
		|| { echo "chart-check FAILED: a rule of the dashboard's key Job names no resource" >&2; exit 1; }; \
	[ "$$(echo "$$rendered" | grep -c 'secretName: check-meridian-runtime-dashboard-signing')" = 1 ] \
		|| { echo "chart-check FAILED: the dashboard's private key is mounted somewhere besides the dashboard" >&2; exit 1; }; \
	echo "$$rendered" | grep -q 'MERIDIAN_DASHBOARD_KEYS_DIR' \
		|| { echo "chart-check FAILED: a sidecar is not given the dashboard's public keys" >&2; exit 1; }; \
	broker="$$(doc Role check-meridian-runtime-broker)"; \
	[ -n "$$broker" ] && ! echo "$$broker" | grep -q '"create"\|"delete"\|"update"' \
		&& [ "$$(echo "$$broker" | grep -A1 'resources: \["secrets"\]' | grep -c 'resourceNames: \["check-meridian-runtime-broker-launched"\]')" = 1 ] \
		|| { echo "chart-check FAILED: the broker's process may do more than read Deployments and write its launched plugins' credentials:" >&2; echo "$$broker" >&2; exit 1; }; \
	launcher="$$(doc Role check-meridian-runtime-launcher)"; \
	[ -n "$$launcher" ] && [ "$$(echo "$$launcher" | grep -c 'resources:')" = 2 ] \
		&& echo "$$launcher" | grep -q 'resources: \["deployments"\]' \
		&& echo "$$launcher" | grep -A1 'resources: \["persistentvolumeclaims"\]' | grep -q 'verbs: \["create", "get"\]$$' \
		|| { echo "chart-check FAILED: the launcher may touch something besides Deployments (decisions/019) and make and read edge plugins' storage, never delete it (decisions/028):" >&2; echo "$$launcher" >&2; exit 1; }; \
	template="$$(doc ConfigMap check-meridian-runtime-launcher-template)"; \
	echo "$$template" | grep -q 'meridian.dev/launched' && echo "$$template" | grep -q 'check-meridian-runtime-broker-launched' \
		|| { echo "chart-check FAILED: the launcher's template does not mark its workloads, or takes their credential from anywhere but the launched plugins' Secret" >&2; exit 1; }; \
	$(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check \
		--api-versions admissionregistration.k8s.io/v1/ValidatingAdmissionPolicy 2>/dev/null | grep -q '^kind: ValidatingAdmissionPolicyBinding$$' \
		|| { echo "chart-check FAILED: on a cluster with ValidatingAdmissionPolicy, nothing holds the launcher to its own Deployments" >&2; exit 1; }; \
	doc Deployment check-meridian-runtime-sidecar-check-1 | grep -q '^    type: Recreate$$' \
		|| { echo "chart-check FAILED: a plugin's Deployment rolls, so two copies of one instance would run under one name" >&2; exit 1; }; \
	echo "$$policy" | grep -q 'meridian.dev/component: sidecar' \
		|| { echo "chart-check FAILED: the plugins' NetworkPolicy does not select every sidecar by its component" >&2; exit 1; }; \
	echo "$$rendered" | grep -q '^      subdomain: check-meridian-runtime-sidecars$$' \
		|| { echo "chart-check FAILED: a sidecar pod is not named under the plugins' one Service, so the dashboard cannot reach it" >&2; exit 1; }; \
	[ "$$(echo "$$rendered" | grep -c '^kind: NetworkPolicy$$')" = "$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null | grep -c '^kind: NetworkPolicy$$')" ] \
		|| { echo "chart-check FAILED: a plugin brought its own NetworkPolicy; every plugin shares one, so the launcher never makes one" >&2; exit 1; }
	@$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| awk '/^kind: Job$$/{j=1} j&&/helm.sh\/hook/{print} /^---/{j=0}' | grep -q 'pre-install\|pre-upgrade\|post-install' \
		&& { echo "chart-check FAILED: a Job runs as a Helm hook. A hook must finish before the dashboard exists, and on a fresh install the wizard is what configures the database it would wait for" >&2; exit 1; }; \
	true
	@# Upgrades leave nothing behind to pile up (task kernel/upgrading-a-
	@# deployment-in-place): four upgrades of one deployment left 26 old
	@# ReplicaSets and Jobs from earlier revisions. Every Deployment keeps three
	@# old ReplicaSets rather than Kubernetes' ten. Every Job goes once it is
	@# done: a hook when it succeeds, any other by carrying its revision in its
	@# name, so the next upgrade removes it, and by a TTL, for the Jobs of an
	@# upgrade that failed, which no later one removes. Rendered with a plugin,
	@# so every Deployment and Job the chart has is in it.
	@rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check \
		--set 'sidecars[0].instanceId=check-1' 2>/dev/null)"; \
	[ "$$(echo "$$rendered" | grep -cE '^kind: (Deployment|Job)$$')" -ge 12 ] \
		|| { echo "chart-check FAILED: the render has fewer Deployments and Jobs than the chart makes, so the check below would prove little" >&2; exit 1; }; \
	left="$$(echo "$$rendered" | awk ' \
		function judge() { \
			if (kind == "Deployment" && !limit) print "  the Deployment " name " keeps ten old ReplicaSets; give it revisionHistoryLimit: 3"; \
			if (kind == "Job" && hook && !succeeded) print "  the hook Job " name " stays after it succeeds; give it hook-delete-policy hook-succeeded"; \
			if (kind == "Job" && !hook && name !~ /-1$$/) print "  the Job " name " is not named by its revision, so no upgrade removes it"; \
			if (kind == "Job" && !hook && !ttl) print "  the Job " name " has no ttlSecondsAfterFinished, so one a failed upgrade left stays for good"; \
			kind = ""; name = ""; limit = 0; hook = 0; succeeded = 0; ttl = 0 \
		} \
		/^---/ { judge(); next } \
		/^kind: / { kind = $$2 } \
		/^  name: / && name == "" { name = $$2 } \
		/^  revisionHistoryLimit: 3$$/ { limit = 1 } \
		/helm.sh\/hook"?:/ { hook = 1 } \
		/helm.sh\/hook-delete-policy"?:.*hook-succeeded/ { succeeded = 1 } \
		/^  ttlSecondsAfterFinished: / { ttl = 1 } \
		END { judge() }')"; \
	[ -z "$$left" ] || { echo "chart-check FAILED: an upgrade would leave these behind:" >&2; echo "$$left" >&2; exit 1; }
	@# A binding a Job deletes when it gives its rights up (decisions/016) is a
	@# hook, never a resource of the release: `helm upgrade --wait` waits for
	@# every resource the release names to exist, and failed on one of these
	@# being gone (task kernel/upgrading-a-deployment-in-place, fault 2).
	@rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null)"; \
	given_up="$$(echo "$$rendered" | grep -A1 'resources: \["rolebindings"\]' | grep resourceNames | tr -d '[]",' | sed 's/.*resourceNames://')"; \
	[ "$$(echo $$given_up | wc -w | tr -d ' ')" -ge 3 ] \
		|| { echo "chart-check FAILED: found fewer bindings given up than the three Jobs that give theirs up, so the check below would prove little" >&2; exit 1; }; \
	for binding in $$given_up; do \
		echo "$$rendered" | awk -v n="$$binding" '/^---/{b=0;m=0} /^kind: RoleBinding$$/{b=1} $$0=="  name: "n{m=1} b&&m&&/helm.sh\/hook"?:.*pre-install,pre-upgrade/{f=1} END{exit !f}' \
			|| { echo "chart-check FAILED: the RoleBinding $$binding is deleted by its Job and is not a pre-install,pre-upgrade hook, so helm upgrade --wait fails on it being gone" >&2; exit 1; }; \
	done
	@# One conductor at a time. A rolling update that surges keeps the old one
	@# answering the bus until the new one is ready, and the new one is not
	@# ready while it waits for its schema: after the wizard's apply the
	@# dashboard read the access records from the old one's first-run store,
	@# which holds nobody, and the administrator the wizard named signed in to
	@# a home page saying nobody administers it (task
	@# kernel/upgrading-a-deployment-in-place). No surge, not Recreate: Helm 4's
	@# server-side apply cannot change an existing Deployment to Recreate.
	@$(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null \
		| awk '/^---/{d=0;c=0;next} /^kind: Deployment$$/{d=1} d&&$$0=="  name: check-meridian-runtime-conductor"{c=1} c&&/^      maxSurge: 0$$/{f=1} END{exit !f}' \
		|| { echo "chart-check FAILED: the conductor's Deployment surges, so an old conductor answers beside the new one while it waits for its schema" >&2; exit 1; }
	@# One writer per partition of the book (W9.8): its rollout does not surge
	@# either, so an old book never journals beside the new one.
	@$(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check 2>/dev/null \
		| awk '/^---/{d=0;c=0;next} /^kind: Deployment$$/{d=1} d&&$$0=="  name: check-meridian-runtime-bor"{c=1} c&&/^      maxSurge: 0$$/{f=1} END{exit !f}' \
		|| { echo "chart-check FAILED: the book's Deployment surges, so two of it would write one partition's journal" >&2; exit 1; }
	@# The Ingress (spec/live-plugin-development, ruling 1): none unless asked
	@# for; asked for, two names to the dashboard's one port, the plugins' a
	@# wildcard below the host; TLS for both; the address offered to the
	@# wizard; and a host that is an address, or none, refused.
	@base="--set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check"; \
	[ "$$($(HELM) template check deploy/chart $$base 2>/dev/null | grep -c '^kind: Ingress$$')" = 0 ] \
		|| { echo "chart-check FAILED: an Ingress is rendered though none was asked for" >&2; exit 1; }; \
	on="$$($(HELM) template check deploy/chart $$base --set ingress.enabled=true --set ingress.host=meridian.firm.example \
		--set ingress.tls.secretName=dash-tls --set ingress.tls.pluginsSecretName=plugins-tls 2>/dev/null)" \
		|| { echo "chart-check FAILED: the chart does not render with its Ingress on" >&2; exit 1; }; \
	ingress="$$(echo "$$on" | awk '/^---/{p=0} /^kind: Ingress$$/{p=1} p')"; \
	echo "$$ingress" | grep -q '^    - host: "meridian.firm.example"$$' \
		&& echo "$$ingress" | grep -q '^    - host: "\*.plugins.meridian.firm.example"$$' \
		&& [ "$$(echo "$$ingress" | grep -c 'name: check-meridian-runtime-dashboard$$')" = 3 ] \
		|| { echo "chart-check FAILED: the Ingress does not route the host and the plugins' wildcard to the dashboard:" >&2; echo "$$ingress" >&2; exit 1; }; \
	echo "$$ingress" | grep -q 'secretName: "dash-tls"' && echo "$$ingress" | grep -q 'secretName: "plugins-tls"' \
		|| { echo "chart-check FAILED: the Ingress's TLS does not cover both names" >&2; exit 1; }; \
	echo "$$ingress" | grep -q 'proxy-body-size: "0"' \
		|| { echo "chart-check FAILED: the Ingress would refuse an upload over nginx-ingress's default megabyte" >&2; exit 1; }; \
	echo "$$on" | grep -A1 'MERIDIAN_DASHBOARD_SUGGESTED_URL' | grep -q '"https://meridian.firm.example"' \
		|| { echo "chart-check FAILED: the wizard is not offered the address the Ingress serves" >&2; exit 1; }; \
	for bad in "--set ingress.host=10.0.0.7" "--set ingress.host="; do \
		! $(HELM) template check deploy/chart $$base --set ingress.enabled=true --set ingress.tls.secretName=dash-tls $$bad >/dev/null 2>&1 \
			|| { echo "chart-check FAILED: an Ingress rendered with $$bad, where plugin pages cannot have names below it" >&2; exit 1; }; \
	done
	@# HTTPS alone unless plain HTTP is turned on, for testing (task kernel/a-
	@# development-deployment-serves-https, ruling 3): no certificate and no
	@# ingress.plainHttp is refused, saying both; with a certificate, Traefik is
	@# kept to websecure; plain HTTP on is served on every entrypoint, and the
	@# wizard is offered http.
	@base="--set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check --set ingress.enabled=true --set ingress.host=meridian.localhost"; \
	refused="$$($(HELM) template check deploy/chart $$base 2>&1)"; \
	[ $$? -ne 0 ] && echo "$$refused" | grep -q 'ingress.tls.secretName is empty' && echo "$$refused" | grep -q 'ingress.plainHttp=true' \
		|| { echo "chart-check FAILED: an Ingress with no certificate rendered with plain HTTP off, or was refused without saying how to fix it:" >&2; echo "$$refused" | tail -3 >&2; exit 1; }; \
	served="$$($(HELM) template check deploy/chart $$base --set ingress.tls.secretName=meridian-tls 2>/dev/null | awk '/^---/{p=0} /^kind: Ingress$$/{p=1} p')"; \
	echo "$$served" | grep -q 'traefik.ingress.kubernetes.io/router.entrypoints: "websecure"' \
		|| { echo "chart-check FAILED: with plain HTTP off, Traefik would serve the Ingress on port 80 too" >&2; exit 1; }; \
	plain="$$($(HELM) template check deploy/chart $$base --set ingress.plainHttp=true 2>/dev/null)" \
		|| { echo "chart-check FAILED: with ingress.plainHttp on, an Ingress with no certificate does not render" >&2; exit 1; }; \
	! echo "$$plain" | grep -q 'router.entrypoints' \
		|| { echo "chart-check FAILED: with ingress.plainHttp on, Traefik is still kept off port 80" >&2; exit 1; }; \
	echo "$$plain" | grep -A1 'MERIDIAN_DASHBOARD_SUGGESTED_URL' | grep -q '"http://meridian.localhost"' \
		|| { echo "chart-check FAILED: with plain HTTP on and no certificate, the wizard is not offered the http address the Ingress serves" >&2; exit 1; }
	@# Development (spec/live-plugin-development, ruling 2): only when asked.
	@base="--set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check"; \
	! $(HELM) template check deploy/chart $$base 2>/dev/null | grep -q MERIDIAN_DEVELOPMENT \
		|| { echo "chart-check FAILED: a deployment nobody installed for development is told it is one" >&2; exit 1; }; \
	$(HELM) template check deploy/chart $$base --set development=true 2>/dev/null | grep -A1 MERIDIAN_DEVELOPMENT | grep -q '"true"' \
		|| { echo "chart-check FAILED: a deployment installed for development does not tell its dashboard" >&2; exit 1; }
	@# The live shape (spec/live-plugin-development): the launcher's second
	@# template on a development deployment alone, with the shared folder, the
	@# shared group, the step seeding it group-writable and the dev runner; the
	@# plugin shape unchanged by it.
	@base="--set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check"; \
	! $(HELM) template check deploy/chart $$base 2>/dev/null | grep -q 'plugin-live.json' \
		|| { echo "chart-check FAILED: a deployment not for development renders the live shape" >&2; exit 1; }; \
	live="$$($(HELM) template check deploy/chart $$base --set development=true 2>/dev/null | grep '^  plugin-live.json:')"; \
	for said in 'meridian-dev' 'fsGroup' '/plugin/live' 'MERIDIAN_LIVE_DIR' 'meridian.dev/live' 'meridian.dev/launched' 'initContainers' 'g+rwX' 'mindepth'; do \
		echo "$$live" | grep -q "$$said" \
			|| { echo "chart-check FAILED: the live shape does not carry $$said" >&2; exit 1; }; \
	done; \
	plain="$$($(HELM) template check deploy/chart $$base --set development=true 2>/dev/null | grep '^  plugin.json:')"; \
	! echo "$$plain" | grep -q 'meridian-dev\|/plugin/live\|fsGroup\|initContainers' \
		|| { echo "chart-check FAILED: the plugin shape carries the live shape's parts" >&2; exit 1; }
	@echo "chart-check OK: four components, the dashboard and the three ways it signs people in, the key and the settings key on the conductor alone, both key paths, refusals, migrations, no pinned uid, a plugin held to its side of the pod, its front door open to the dashboard alone, an Ingress only when asked for and HTTPS alone unless plain HTTP is, development only when asked for, the live shape there alone, and nothing an upgrade leaves behind"

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

# Migration before start, because a starting runtime verifies the street store's
# schema and refuses to serve against one it does not recognise. One command
# per release rather than every process racing to apply the same change.
migrate: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@$(COMPOSE) up -d postgres >/dev/null
	@$(COMPOSE) run --rm --build -T instrument meridian-instrument migrate
	@$(COMPOSE) run --rm --build -T street meridian-street migrate
	@$(COMPOSE) run --rm --build -T bor meridian-bor migrate
	@$(COMPOSE) run --rm --build -T conductor meridian-conductor migrate

up: migrate
	@$(COMPOSE) up --build

down:
	@$(COMPOSE) down -v

# The Python SDK against this runtime.
#
# The sidecar surface is implemented twice, once here and once in the SDK, and
# decisions/007 says the two must agree. Nothing checked that until this target:
# each side's tests drove a server written in the same language by the same
# hand, which shows each is self-consistent and nothing more.
#
# A deployment id is supplied because the runtime requires one, and any value
# does: the platform is not contacted at startup, and this check never reaches
# it. The role and the grants come from compose and the mounted example table,
# so what the SDK is held to here is the file a deployment actually ships.
#
# The SDK's image is handed this working tree's proto/ as its core-proto build
# context, in place of the core commit the SDK pins, so the domain messages its
# tests encode are the ones this runtime was built from.
#
# The suite's rows reach the street store only for an account linked and
# writable, which a deployment admin makes; e2e/interop/a-linked-account.sql
# makes it here. What the store kept is then read back from Postgres and held
# to e2e/interop/positions.expected, character for character: the numbers the
# SDK sent, at the scale they were stated with (decisions/023); each position's
# side, and a settle-date quantity or a market value only where one was sent;
# the rows whose currency was the connector's assumption; the positions the
# venue also counts in cash; and each venue shape's statement figures as sent
# (spec/the-account-side-fits-every-venue).
#
# Two `operations` plugins' sidecars run beside the custody one (contract v7):
# the suite reads and hears the street through the first, whose read scope
# holds the interop account, and through the second, whose scope is empty,
# reads and hears nothing.
#
# Read by the plugin harness's street.sql, the one statement of the format a
# plugin's e2e compares against, so this holds it too. Ordered bytewise, so
# the file does not depend on the database's collation. Only the suite's
# account: the database is the one `make test-store` and the others leave
# their rows in. Each venue shape's statement is its source's latest; the
# suite's own source, `interop`, opens many, every one read at the same fixed
# time, so which is latest is nothing the suite states, and its line is left
# out.
SDK ?= ../meridian-python

interop: network
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "interop FAILED: the SDK's image did not build. See it with:" >&2; \
		     echo "  DOCKER_BUILDKIT=1 docker build --build-context core-proto=$(CURDIR)/proto $(SCHEMA_PROTO) -f $(SDK)/Dockerfile.python --target interop --progress=plain $(SDK)" >&2; exit 1; }
	@$(COMPOSE) up -d postgres >/dev/null 2>&1
	@$(COMPOSE) run --rm --build -T instrument meridian-instrument migrate
	@# Each with --build: a service's image is its own tag, and one left from
	@# an earlier run would apply an earlier release's migrations.
	@{ $(COMPOSE) run --rm --build -T street meridian-street migrate \
	   && $(COMPOSE) run --rm --build -T conductor meridian-conductor migrate; } >/dev/null 2>&1 \
		|| { echo "interop FAILED: the schema could not be applied" >&2; exit 1; }
	@$(COMPOSE) exec -T postgres psql -U meridian -d meridian -v ON_ERROR_STOP=1 -q \
		<e2e/interop/a-linked-account.sql >/dev/null \
		|| { echo "interop FAILED: the linked account could not be written" >&2; exit 1; }
	@# All three components, because the surface under test is the sidecar's
	@# and the answers come from the other two across a broker. Before the
	@# split this was one process, and the test could not tell the difference.
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@$(COMPOSE) up -d nats >/dev/null 2>&1 && $(COMPOSE) restart nats >/dev/null 2>&1
	@MERIDIAN_DEPLOYMENT_ID=DEP-interop $(COMPOSE) --profile interop up -d --build street instrument conductor sidecar sidecar-operations sidecar-operations-unscoped >/dev/null 2>&1 \
		|| { echo "interop FAILED: the components did not start" >&2; exit 1; }
	@# --no-deps: everything the suite reaches is up already, and the three
	@# sidecars share the custody one's network namespace. A run that started
	@# its dependencies could recreate that sidecar -- Compose 2.37.1 to 2.38.x
	@# leave a Bake-built container's image label empty and recreate it on the
	@# next convergence (docker/compose#13047) -- and the suite would join the
	@# new namespace, where 9191 answers and 9192 and 9193 never do.
	@MERIDIAN_DEPLOYMENT_ID=DEP-interop $(COMPOSE) run --rm --no-deps -T interop \
		python -m pytest -q tests/test_interop.py >.interop.log 2>&1; \
		status=$$?; \
		$(COMPOSE) exec -T postgres psql -U meridian -d meridian -At -v ON_ERROR_STOP=1 \
			<deploy/harness/street.sql 2>>.interop.log \
			| awk -F'|' '$$2 == "Interop" && !($$1 == "statement" && $$3 == "interop")' \
			>.interop.positions; \
		MERIDIAN_DEPLOYMENT_ID=DEP-interop $(COMPOSE) --profile interop down -v >/dev/null 2>&1; \
		if [ $$status -ne 0 ]; then \
			echo "interop FAILED. The last 40 lines, and the whole of it in .interop.log:" >&2; \
			tail -40 .interop.log >&2; exit 1; \
		fi; \
		diff -u e2e/interop/positions.expected .interop.positions >&2 \
			|| { echo "interop FAILED: the street store did not keep what the SDK sent" >&2; exit 1; }
	@echo "interop OK: the Python SDK and this runtime agree on the sidecar surface, and the street store keeps its numbers exactly"

# The red-team corpus, vendored from meridian-design's evals/prompt-attacks/
# (plans/tickets-inside-a-deployment, Q6, ruled 2026-10-03): one corpus for
# the deployment and the platform, canonical in design, each case unchanged.
# Core's tests replay every case a deployment channel carries against the
# sidecar's and the dashboard's rules, and `make e2e-tickets` files them on
# the plugin harness. Regenerated here, never edited: run it again when
# design's corpus changes, and commit the file it writes.
DESIGN ?= ../meridian-design
PROMPT_ATTACKS := deploy/prompt-attacks.json

prompt-attacks:
	@test -d "$(DESIGN)/evals/prompt-attacks" \
		|| { echo "no corpus at $(DESIGN)/evals/prompt-attacks; set DESIGN=<path to meridian-design>" >&2; exit 1; }
	@$(PY) -c 'import json, pathlib, subprocess, sys; root = pathlib.Path(sys.argv[1]); corpus = json.loads((root / "corpus.json").read_text()); cases = sorted((json.loads(p.read_text()) for p in root.glob("RT*/case.json")), key=lambda c: int(c["id"][2:])); rev = subprocess.run(["git", "-C", str(root), "log", "-1", "--format=%h", "--", "."], capture_output=True, text=True).stdout.strip(); out = {"about": "Vendored from meridian-design evals/prompt-attacks at " + rev + " by make prompt-attacks; never edited here.", "channels": corpus["channels"], "outcomes": corpus["outcomes"], "refusals": corpus["refusals"], "cases": cases}; pathlib.Path(sys.argv[2]).write_text(json.dumps(out, indent=1, ensure_ascii=True) + "\n")' "$(DESIGN)/evals/prompt-attacks" "$(PROMPT_ATTACKS)"
	@echo "prompt-attacks: wrote $(PROMPT_ATTACKS) from $(DESIGN)/evals/prompt-attacks"

# The book of record (W9, contract v8), end to end through the Python SDK.
#
# Every component, across the broker: the street store a custody plugin's
# sidecar records statements into, the book an operations plugin's sidecar
# writes for a person, and a reporting plugin's sidecar reading and hearing it
# within its scope, beside an operations plugin's whose scope is empty. The
# SDK's book suite (meridian-python tests/test_book.py) drives them: day 1, an
# opening balance composed from the street and confirmed for a person, read
# back by reporting, a second refused by its code and a duplicate applied
# once; day 2, the custodian's change a break heard with its cause and own,
# the figures per agreement, an adjustment resolving it and moving the book
# into agreement with the custodian, an injected difference closed with an
# explanation and another as cleared; scope isolation; a stream killed and
# caught up. e2e/book/accounts.sql makes the accounts and the grants a
# deployment admin would. The person's assertion is signed with a key made
# for the run, whose public half only the operations sidecar holds.
#
# Then `meridian-bor rebuild`, and the book as deploy/harness/book.sql prints it
# before and after must be the same, character for character: positions,
# lots, pending settlements, breaks, figures and entries.
e2e-book: network
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" $(SCHEMA_PROTO) -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "e2e-book FAILED: the SDK's image did not build" >&2; exit 1; }
	@$(COMPOSE) --profile book down -v --remove-orphans >/dev/null 2>&1 || true
	@$(COMPOSE) up -d postgres >/dev/null 2>&1
	@{ $(COMPOSE) run --rm --build -T instrument meridian-instrument migrate \
	   && $(COMPOSE) run --rm --build -T street meridian-street migrate \
	   && $(COMPOSE) run --rm --build -T bor meridian-bor migrate \
	   && $(COMPOSE) run --rm --build -T conductor meridian-conductor migrate; } >.e2e-book.log 2>&1 \
		|| { echo "e2e-book FAILED: the schema could not be applied; see .e2e-book.log" >&2; exit 1; }
	@$(COMPOSE) exec -T postgres psql -U meridian -d meridian -v ON_ERROR_STOP=1 -q \
		<e2e/book/accounts.sql >/dev/null \
		|| { echo "e2e-book FAILED: the accounts and grants could not be written" >&2; exit 1; }
	@$(COMPOSE) exec -T postgres psql -U meridian -d meridian -v ON_ERROR_STOP=1 -q \
		<e2e/book/instruments.sql >/dev/null \
		|| { echo "e2e-book FAILED: the instrument records could not be written" >&2; exit 1; }
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@$(COMPOSE) up -d nats >/dev/null 2>&1 && $(COMPOSE) restart nats >/dev/null 2>&1
	@MERIDIAN_DEPLOYMENT_ID=DEP-book $(COMPOSE) --profile book up -d --build street instrument conductor bor \
		sidecar-book sidecar-book-operations sidecar-book-reporting sidecar-book-unscoped >>.e2e-book.log 2>&1 \
		|| { echo "e2e-book FAILED: the components did not start; see .e2e-book.log" >&2; exit 1; }
	@# --no-deps, as interop: everything the suite reaches is up already, and a
	@# run that converged its dependencies could recreate sidecar-book under
	@# Compose 2.38.x and leave the suite in a namespace nothing answers in.
	@MERIDIAN_DEPLOYMENT_ID=DEP-book $(COMPOSE) --profile book run --rm --no-deps -T book \
		python -m pytest -q tests/test_book.py >>.e2e-book.log 2>&1; \
		status=$$?; \
		$(COMPOSE) exec -T postgres psql -U meridian -d meridian -At -v ON_ERROR_STOP=1 \
			<deploy/harness/book.sql >.e2e-book.before 2>>.e2e-book.log; \
		$(COMPOSE) stop bor >>.e2e-book.log 2>&1; \
		$(COMPOSE) run --rm --no-deps -T bor meridian-bor rebuild >>.e2e-book.log 2>&1 || status=$$?; \
		$(COMPOSE) exec -T postgres psql -U meridian -d meridian -At -v ON_ERROR_STOP=1 \
			<deploy/harness/book.sql >.e2e-book.after 2>>.e2e-book.log; \
		$(COMPOSE) --profile book logs --no-color >>.e2e-book.log 2>&1; \
		MERIDIAN_DEPLOYMENT_ID=DEP-book $(COMPOSE) --profile book down -v >/dev/null 2>&1; \
		if [ $$status -ne 0 ]; then \
			echo "e2e-book FAILED. The suite's last 60 lines, and every component's in .e2e-book.log:" >&2; \
			grep -v '^[a-z-]*-1  |' .e2e-book.log | tail -60 >&2; exit 1; \
		fi; \
		grep -q '^position|' .e2e-book.before \
			|| { echo "e2e-book FAILED: the book printed no position, so the rebuild proves nothing" >&2; exit 1; }; \
		diff -u .e2e-book.before .e2e-book.after >&2 \
			|| { echo "e2e-book FAILED: meridian-bor rebuild did not reproduce the book" >&2; exit 1; }
	@echo "e2e-book OK: through the SDK, an opening balance for a person, breaks heard with their cause, figures per agreement, an adjustment resolving a break and moving the book, closes by explanation and as cleared, refusals by their code, scope isolation and a stream caught up; meridian-bor rebuild reproduces the book"

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
	@$(MAKE) --no-print-directory migrate
	@echo "1/4  making sure this deployment has a key"
	@mkdir -p .demo
	@$(COMPOSE) run --rm --no-deps -T conductor meridian-conductor public-key > .demo/public-key.pem
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

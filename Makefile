SHELL := /bin/bash
PY     := python3

RUST_VERSION := 1.90
COMPOSE := docker compose
DOCKER := DOCKER_BUILDKIT=1 docker

.PHONY: migrate test-broker nats-permissions check-nats-permissions help ci-local ci-local-deep install-hooks ci-mirror-check \
        e2e-first-run-brought e2e-first-run-oidc e2e-cluster e2e-cluster-external \
        test-directory e2e-dashboard-oidc e2e-dashboard-ldap e2e-dashboard-accounts \
        build test test-store chart-check check-crate-boundaries check-test-targets check-local-storage \
        check-chart-files \
        interop lint fmt lock contract-diff up down demo network codegen check-codegen advisories e2e-first-run

help:
	@echo "  make ci-local       run every gate (the pre-push gate, and what CI mirrors)"
	@echo "  make codegen        regenerate the domain bindings from proto/"
	@echo "  make check-codegen  fail if crates/domain/src/v1.rs is stale against proto/"
	@echo "  make build          compile the workspace"
	@echo "  make test           run the unit tests"
	@echo "  make test-store     run the Postgres store's tests against Postgres"
	@echo "  make chart-check    lint the Helm chart, and check that it refuses bad values"
	@echo "  make check-crate-boundaries  nothing links against another component's store"
	@echo "  make check-test-targets      every integration test is named by a target that runs it"
	@echo "  make check-local-storage     the development cluster keeps its database across a restart"
	@echo "  make up             bring up Postgres and the runtime"
	@echo "  make down           take them down, keeping nothing"
	@echo "  make demo           register this deployment and prove the round trip"
	@echo "  make lint           rustfmt --check and clippy with warnings denied"
	@echo "  make lock           regenerate Cargo.lock"
	@echo "  make install-hooks  point git at hooks/ so push fires ci-local"

# Local green is the completion signal; CI is confirmation.
ci-local: contract-diff ci-mirror-check check-crate-boundaries check-test-targets check-chart-files check-local-storage check-nats-permissions check-codegen advisories build test test-store test-broker test-directory interop e2e-dashboard-oidc e2e-dashboard-ldap e2e-dashboard-accounts e2e-first-run e2e-first-run-brought e2e-first-run-oidc chart-check lint
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

# A tests/ file compiles into its own binary and runs only when a target names
# it. The runtime's wiring test was named by nothing and never ran, while both
# local and CI reported green. This refuses the next one.
check-test-targets:
	@$(PY) tools/check_test_targets.py --self-test --repo-root .
	@$(PY) tools/check_test_targets.py --repo-root .

# A development cluster's database is a database somebody has work in.
#
# It ran on an emptyDir for a day, which is the pod's own storage: the first
# DiskPressure eviction took the schema and both roles with it, and what
# reported the loss was the platform noticing the deployment had gone quiet.
# Nothing failed at the moment the mistake was made, which is the shape of
# defect this repository keeps writing gates for.
#
# The key, not the word: the first version of this matched the string, and
# the sentence explaining why the volume is a claim failed the gate.
# The chart ships its own copy of the topic registry, because a cluster cannot
# read meridian-design and the broker generates its permissions from it. A
# second copy of a fact is a copy that goes stale, and this one did: the
# registry gained a topic, deploy/topics.tsv was regenerated, the chart's was
# not, and the deployment that came up refused its own conductor's
# subscription with a permissions violation. Nothing compared them until now.
check-chart-files:
	@diff -q deploy/topics.tsv deploy/chart/files/topics.tsv >/dev/null \
		|| { echo "check-chart-files FAILED: the chart's topic registry is not the one this repo uses" >&2; \
		     diff deploy/topics.tsv deploy/chart/files/topics.tsv >&2; \
		     echo "  cp deploy/topics.tsv deploy/chart/files/topics.tsv" >&2; exit 1; }
	@echo "check-chart-files OK: the chart ships the topic registry this repo uses"

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

# Regenerate in place. The only sanctioned way to change $(DOMAIN_RS).
codegen:
	@rm -rf $(SCRATCH)
	@$(DOCKER) build -f Dockerfile.codegen --target export \
		--output type=local,dest=$(SCRATCH) .
	@mv $(SCRATCH)/v1.rs $(DOMAIN_RS) && rm -rf $(SCRATCH)
	@echo "codegen: wrote $(DOMAIN_RS)"

# Generate into a scratch directory and compare. A vendored file that does not
# match a fresh generation is a stale checkout or a hand-edit, and both are the
# same bug: the runtime building against types proto/ does not describe.
check-codegen:
	@rm -rf $(SCRATCH)
	@$(DOCKER) build -f Dockerfile.codegen --target export \
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
			-p meridian-config --test postgres -p meridian-runtime --test grants \
		>.test-store.log 2>&1 \
		|| { echo "test-store FAILED. The last 40 lines, and the whole of it in .test-store.log:" >&2; \
		     tail -40 .test-store.log >&2; exit 1; }
	@echo "test-store OK: the three stores pass against Postgres, and the migration grants the serving role what it made"

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

# Any of the four cluster runs, on a k3d cluster made for it and removed after:
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
# CoreDNS is restarted once it is made. k3d adds the name after CoreDNS has
# started, and CoreDNS reads that file through a mount that never updates:
# without the restart the name resolves to nothing on a runner, and to the
# desktop's own answer on a laptop, which hides the defect.
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

e2e-cluster-k3d:
	@docker network inspect $(E2E_K3D_NETWORK) >/dev/null 2>&1 \
		|| docker network create --subnet $(E2E_K3D_SUBNET) --gateway $(E2E_K3D_GATEWAY) $(E2E_K3D_NETWORK) >/dev/null
	@$(K3D) cluster delete $(E2E_K3D_CLUSTER) >/dev/null 2>&1 || true
	@echo "e2e-cluster-k3d: a cluster for $(E2E_CLUSTER_TARGET)"
	@$(K3D) cluster create $(E2E_K3D_CLUSTER) --network $(E2E_K3D_NETWORK) \
		--host-alias $(E2E_K3D_GATEWAY):host.docker.internal \
		--api-port $(E2E_K3D_API) --k3s-arg "--disable=traefik@server:0" --wait >/dev/null
	@$(K3D) kubeconfig get $(E2E_K3D_CLUSTER) > $(E2E_K3D_KUBECONFIG)
	@KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n kube-system rollout restart deploy/coredns >/dev/null
	@KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n kube-system rollout status deploy/coredns --timeout=120s >/dev/null
	@KUBECONFIG=$(E2E_K3D_KUBECONFIG) $(MAKE) --no-print-directory $(E2E_CLUSTER_TARGET) \
		E2E_IMAGE_LOAD="$(K3D) image import -c $(E2E_K3D_CLUSTER)"; \
	  held=$$?; \
	  if [ $$held -ne 0 ]; then \
	    echo "e2e-cluster-k3d: what the cluster said, in .e2e-cluster.log" >&2; \
	    { KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl get pods -A -o wide; \
	      for pod in $$(KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) get pods -o name); do \
	        echo "== $$pod"; \
	        KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) logs $$pod --all-containers --tail=80; \
	        echo "== $$pod, the container before, if it restarted"; \
	        KUBECONFIG=$(E2E_K3D_KUBECONFIG) kubectl -n $(E2E_CLUSTER_NAMESPACE) logs $$pod --all-containers --previous --tail=80; \
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
PLUGIN_VALUES := --set 'sidecars[0].instanceId=custody-1' --set 'sidecars[0].role=custody' --set grants.existingConfigMap=g \
	--set 'sidecars[0].plugin.image=example/plugin:1' --set 'sidecars[0].plugin.existingSecret=vendor'

# A chart that renders is half the check. The other half is that it refuses:
# a component with no deployment identifier, no key or no database installs
# happily and then crash-loops, and the operator reads a restart count instead
# of a sentence.
# The broker's permissions, from the grant table. Decision 010.
# The broker's permissions, generated by the runtime's own binary.
#
# One implementation of the policy: a bundled broker generates its own
# configuration from the same code at start, so a file here and a chart there
# cannot drift into two policies that must agree.
RUNTIME_IMAGE = meridian-runtime:local
BROKER_CONFIG = $(DOCKER) run --rm -v "$(CURDIR)":/w -w /w $(RUNTIME_IMAGE) meridian-broker-config

runtime-image:
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null

# The committed file is the harness's: it carries the roles and instances this
# repository launches for its own tests, merged over the ones core launches
# itself. A deployment's own broker is generated in its cluster from its own
# grant table (templates/broker.yaml), and never from this.
nats-permissions: runtime-image
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
		--out /w/deploy/nats/permissions.conf
	@echo "nats-permissions: wrote deploy/nats/permissions.conf"

check-nats-permissions: runtime-image
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
		--out /w/.permissions.check.conf
	@diff -q deploy/nats/permissions.conf .permissions.check.conf >/dev/null \
		|| { echo "check-nats-permissions FAILED: deploy/nats/permissions.conf has drifted from the grant table. Run make nats-permissions" >&2; \
		     rm -f .permissions.check.conf; exit 1; }
	@rm -f .permissions.check.conf
	@echo "nats-permissions OK: deploy/nats/permissions.conf matches the grant table"

# The bus across a process boundary, against a real broker. Decision 010.
#
# Two backends on one broker is the whole point: an in-process test proves
# routing, which the memory backend already does, and proves nothing about a
# message leaving a process.
test-broker: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
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

e2e-dashboard-ldap: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@: >.e2e-dashboard-ldap.log
	@$(E2E_LDAP) down -v --remove-orphans >>.e2e-dashboard-ldap.log 2>&1 || true
	@set -e; \
	$(E2E_LDAP) build dashboard conductor >>.e2e-dashboard-ldap.log 2>&1; \
	$(E2E_LDAP) up -d postgres nats ldap fake-platform >>.e2e-dashboard-ldap.log 2>&1; \
	for i in $$(seq 1 60); do $(E2E_LDAP) logs ldap 2>&1 | grep -q "slapd starting" && break; sleep 1; done; \
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
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
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

e2e-dashboard-accounts: network
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@: >.e2e-dashboard-accounts.log
	@$(E2E_ACCOUNTS) down -v --remove-orphans >>.e2e-dashboard-accounts.log 2>&1 || true
	@set -e; \
	$(E2E_ACCOUNTS) build dashboard conductor >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) up -d postgres nats fake-platform >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T conductor meridian-conductor migrate >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T dashboard meridian-dashboard migrate >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) up -d conductor dashboard >>.e2e-dashboard-accounts.log 2>&1; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner main; \
	$(E2E_ACCOUNTS) run --rm -T accounts-runner locked
	@$(E2E_ACCOUNTS) down -v --remove-orphans >>.e2e-dashboard-accounts.log 2>&1
	@echo "e2e-dashboard-accounts OK: the account first run made signs somebody in, and enough wrong passwords stop it"

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
	@# A sidecar on the broker this chart brings: it has a credential under its
	@# instance, and the broker knows its role. Every other render here passes
	@# broker.existingSecret, so for weeks the bundled broker read a sidecar's
	@# instance and role from a key the values do not have, gave it neither,
	@# and no sidecar could have started on it.
	@rendered="$$($(HELM) template check deploy/chart --set deployment.id=DEP-check --set deployment.enrolmentCode=ENR-check \
		--set 'sidecars[0].instanceId=check-1' --set 'sidecars[0].role=check-role' 2>/dev/null)"; \
	echo "$$rendered" | grep -q '^  check-1: ' \
		|| { echo "chart-check FAILED: the bundled broker makes no credential for a sidecar's instance" >&2; exit 1; }; \
	echo "$$rendered" | grep -q '"instance_id":"check-1","role":"check-role"' \
		|| { echo "chart-check FAILED: the bundled broker does not know a sidecar's instance and role" >&2; exit 1; }
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
	for store in meridian-conductor meridian-street meridian-instrument meridian-dashboard; do \
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
	@$(HELM) template check deploy/chart $(CHART_VALUES) \
		--set 'sidecars[0].instanceId=custody-1' --set 'sidecars[0].role=custody' \
		--set grants.existingConfigMap=g 2>/dev/null \
		| grep -q "sidecar-custody-1" \
		|| { echo "chart-check FAILED: a configured plugin gets no sidecar" >&2; exit 1; }
	@# A plugin joins its sidecar's pod, and the seam between the two
	@# containers is the boundary: the plugin gets the sidecar's address and
	@# nothing that would let it speak on the bus as itself. Each check reads
	@# only the plugin container's block, so a credential correctly held by the
	@# sidecar beside it cannot satisfy or fail it.
	@$(HELM) template check deploy/chart $(CHART_VALUES) $(PLUGIN_VALUES) --show-only templates/sidecar.yaml 2>/dev/null \
		| grep -q "^        - name: plugin$$" \
		|| { echo "chart-check FAILED: a sidecar given a plugin renders no plugin container" >&2; exit 1; }
	@for forbidden in MERIDIAN_BROKER_URL MERIDIAN_PLUGIN_ROLE MERIDIAN_PLUGIN_TAGS "name: grants"; do \
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
		--set 'sidecars[0].instanceId=custody-1' --set 'sidecars[0].role=custody' --set grants.existingConfigMap=g \
		--show-only templates/sidecar.yaml 2>/dev/null | grep -q "^        - name: plugin$$" \
		&& { echo "chart-check FAILED: a sidecar with no plugin rendered a plugin container" >&2; exit 1; } || true
	@bundled="$$($(HELM) template check deploy/chart --set deployment.id=D --set key.existingSecret=k 2>/dev/null)"; \
	echo "$$bundled" | grep -q 'meridian.dev/component: broker' \
		|| { echo "chart-check FAILED: with no broker secret the chart brings no broker, so an install still needs one written by hand" >&2; exit 1; }; \
	echo "$$bundled" | grep -q 'meridian-broker-config' \
		|| { echo "chart-check FAILED: the bundled broker does not generate its permissions from the grant table" >&2; exit 1; }; \
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
	@$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| awk '/^kind: Job$$/{j=1} j&&/helm.sh\/hook/{print} /^---/{j=0}' | grep -q 'pre-install\|pre-upgrade\|post-install' \
		&& { echo "chart-check FAILED: a Job runs as a Helm hook. A hook must finish before the dashboard exists, and on a fresh install the wizard is what configures the database it would wait for" >&2; exit 1; }; \
	echo "chart-check OK: four components, the dashboard and the three ways it signs people in, the key on the conductor alone, both key paths, refusals, migrations, no pinned uid, and a plugin held to its side of the pod"

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
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@$(COMPOSE) up -d postgres >/dev/null
	@$(COMPOSE) run --rm --build -T instrument meridian-instrument migrate
	@$(COMPOSE) run --rm -T street meridian-street migrate
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
SDK ?= ../meridian-python

interop: network
	@test -d "$(SDK)" \
		|| { echo "no SDK at $(SDK); set SDK=<path to meridian-python>" >&2; exit 1; }
	@$(DOCKER) build --build-context core-proto="$(CURDIR)/proto" -f "$(SDK)/Dockerfile.python" --target interop -t meridian-python-interop "$(SDK)" >/dev/null 2>&1 \
		|| { echo "interop FAILED: the SDK's image did not build. See it with:" >&2; \
		     echo "  DOCKER_BUILDKIT=1 docker build --build-context core-proto=$(CURDIR)/proto -f $(SDK)/Dockerfile.python --target interop --progress=plain $(SDK)" >&2; exit 1; }
	@$(COMPOSE) up -d postgres >/dev/null 2>&1
	@$(COMPOSE) run --rm --build -T instrument meridian-instrument migrate
	@{ $(COMPOSE) run --rm -T street meridian-street migrate \
	   && $(COMPOSE) run --rm --build -T conductor meridian-conductor migrate; } >/dev/null 2>&1 \
		|| { echo "interop FAILED: the schema could not be applied" >&2; exit 1; }
	@# All three components, because the surface under test is the sidecar's
	@# and the answers come from the other two across a broker. Before the
	@# split this was one process, and the test could not tell the difference.
	@DOCKER_BUILDKIT=1 $(DOCKER) build -q -t $(RUNTIME_IMAGE) . >/dev/null
	@$(BROKER_CONFIG) --core-grants /w/deploy/grants.example.json \
		--grants /w/deploy/nats/dev-grants.json \
		--instances /w/deploy/nats/dev-instances.json \
		--dev-users /w/deploy/nats/dev-users.json --out /w/deploy/nats/dev.conf
	@$(COMPOSE) up -d nats >/dev/null 2>&1 && $(COMPOSE) restart nats >/dev/null 2>&1
	@MERIDIAN_DEPLOYMENT_ID=DEP-interop $(COMPOSE) up -d --build street instrument conductor sidecar >/dev/null 2>&1 \
		|| { echo "interop FAILED: the components did not start" >&2; exit 1; }
	@MERIDIAN_DEPLOYMENT_ID=DEP-interop $(COMPOSE) run --rm -T interop \
		python -m pytest -q tests/test_interop.py >.interop.log 2>&1; \
		status=$$?; \
		MERIDIAN_DEPLOYMENT_ID=DEP-interop $(COMPOSE) down -v >/dev/null 2>&1; \
		if [ $$status -ne 0 ]; then \
			echo "interop FAILED. The last 40 lines, and the whole of it in .interop.log:" >&2; \
			tail -40 .interop.log >&2; exit 1; \
		fi
	@echo "interop OK: the Python SDK and this runtime agree on the sidecar surface"

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

SHELL := /bin/bash
PY     := python3

RUST_VERSION := 1.90
COMPOSE := docker compose
DOCKER := DOCKER_BUILDKIT=1 docker

.PHONY: migrate test-broker nats-permissions check-nats-permissions help ci-local ci-local-deep install-hooks ci-mirror-check \
        build test test-store chart-check check-crate-boundaries check-test-targets check-local-storage \
        interop lint fmt lock contract-diff up down demo network codegen check-codegen advisories e2e-dashboard

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
	@echo "  make e2e-dashboard  the dashboard's sign-in and access against a bundled Zitadel and LDAP"
	@echo "  make lint           rustfmt --check and clippy with warnings denied"
	@echo "  make lock           regenerate Cargo.lock"
	@echo "  make install-hooks  point git at hooks/ so push fires ci-local"

# Local green is the completion signal; CI is confirmation.
ci-local: contract-diff ci-mirror-check check-crate-boundaries check-test-targets check-local-storage check-nats-permissions check-codegen advisories build test test-store test-broker interop e2e-dashboard chart-check lint
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
			-p meridian-config --test postgres \
		>.test-store.log 2>&1 \
		|| { echo "test-store FAILED. The last 40 lines, and the whole of it in .test-store.log:" >&2; \
		     tail -40 .test-store.log >&2; exit 1; }
	@echo "test-store OK: the instrument, street and configuration stores pass against Postgres"

HELM := docker run --rm -v "$(CURDIR)":/w -w /w alpine/helm:3.16.2
CHART_VALUES := --set deployment.id=DEP-check --set key.existingSecret=k --set database.existingSecret=d --set broker.existingSecret=b
PLUGIN_VALUES := --set 'sidecars[0].instanceId=custody-1' --set 'sidecars[0].role=custody' --set grants.existingConfigMap=g \
	--set 'sidecars[0].plugin.image=example/plugin:1' --set 'sidecars[0].plugin.existingSecret=vendor'

# A chart that renders is half the check. The other half is that it refuses:
# a component with no deployment identifier, no key or no database installs
# happily and then crash-loops, and the operator reads a restart count instead
# of a sentence.
# The broker's permissions, from the grant table. Decision 010.
nats-permissions:
	@$(PY) tools/nats_permissions.py

check-nats-permissions:
	@$(PY) tools/nats_permissions.py --check

# The bus across a process boundary, against a real broker. Decision 010.
#
# Two backends on one broker is the whole point: an in-process test proves
# routing, which the memory backend already does, and proves nothing about a
# message leaving a process.
test-broker: network
	@$(PY) tools/nats_permissions.py --with-dev-users --out deploy/nats/dev.conf >/dev/null
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

chart-check:
	@$(HELM) lint deploy/chart $(CHART_VALUES) >/dev/null 2>&1 \
		|| { echo "chart-check FAILED: helm lint" >&2; \
		     echo "  docker run --rm -v \"$(CURDIR)\":/w -w /w alpine/helm:3.16.2 lint deploy/chart $(CHART_VALUES)" >&2; exit 1; }
	@$(HELM) template check deploy/chart $(CHART_VALUES) >/dev/null 2>&1 \
		|| { echo "chart-check FAILED: the chart does not render with the three required values" >&2; exit 1; }
	@for missing in deployment.id key.existingSecret; do \
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
	for store in meridian-conductor meridian-street meridian-instrument; do \
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
	@if $(HELM) template check deploy/chart $(CHART_VALUES) --set key.generate=true >/dev/null 2>&1; then \
		echo "chart-check FAILED: a supplied key and a generated one are both accepted" >&2; \
		echo "  they mean opposite things, so accepting both hides which one is in use" >&2; exit 1; \
	fi
	@if $(HELM) template check deploy/chart --set deployment.id=DEP-check \
		--set database.existingSecret=d --set broker.existingSecret=b >/dev/null 2>&1; then \
		echo "chart-check FAILED: the chart rendered with neither a key nor key.generate" >&2; exit 1; \
	fi
	@for component in street instrument conductor; do \
		$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
			| grep -q "meridian-$$component\"\]" \
			|| { echo "chart-check FAILED: nothing starts meridian-$$component" >&2; exit 1; }; \
	done
	@$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| grep -c "^kind: Deployment" | grep -q "^3$$" \
		|| { echo "chart-check FAILED: the street store, the instrument store and the conductor are not three Deployments" >&2; \
		     echo "  one workload means none can be upgraded without the others" >&2; exit 1; }
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
	@$(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
		--set dashboard.url=https://meridian.example 2>/dev/null \
		| grep -q '"meridian-dashboard"' \
		|| { echo "chart-check FAILED: the dashboard does not render when enabled" >&2; exit 1; }
	@for refused in "dashboard.url=" "dashboard.replicaCount=2"; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) --set dashboard.enabled=true \
			--set dashboard.url=https://meridian.example --set $$refused >/dev/null 2>&1; then \
			echo "chart-check FAILED: the dashboard rendered with $$refused" >&2; exit 1; \
		fi; \
	done
	@bundled="--set dashboard.enabled=true --set dashboard.url=https://meridian.example \
		--set identity.bundled.enabled=true --set zitadel.image.tag=v4.17.3 --set zitadel.login.image.tag=v4.17.3 \
		--set zitadel.zitadel.configmapConfig.ExternalDomain=id.example --set identity.bundled.egress.allowCidrs={10.0.0.0/8}"; \
	rendered="$$($(HELM) template check deploy/chart $(CHART_VALUES) $$bundled 2>/dev/null)" \
		|| { echo "chart-check FAILED: the chart does not render with the bundled Zitadel on" >&2; exit 1; }; \
	echo "$$rendered" | awk '/^# Source: meridian-runtime\/templates\//{own=1;next} /^# Source: /{own=0} own' | grep -q "runAsUser" \
		&& { echo "chart-check FAILED: a Meridian template pins a uid with the bundled Zitadel on" >&2; exit 1; }; \
	echo "$$rendered" | grep -q '"meridian-group-hook", "setup"' \
		|| { echo "chart-check FAILED: the bundled Zitadel has no setup Job" >&2; exit 1; }; \
	job="$$(echo "$$rendered" | awk '/^---/{if(f)print d; d=""; f=0} {d=d $$0 "\n"} /"meridian-group-hook", "setup"/{f=1} END{if(f)print d}')"; \
	echo "$$job" | grep -q 'helm.sh/hook' \
		&& { echo "chart-check FAILED: the setup Job is a hook, and a hook waits on the pods that wait on it" >&2; exit 1; }; \
	echo "$$job" | grep -q 'MERIDIAN_ZITADEL_ADMIN_TOKEN_FILE' \
		|| { echo "chart-check FAILED: the setup Job does not read Zitadel's admin token from its file" >&2; exit 1; }; \
	echo "$$rendered" | grep -A12 'name: check-zitadel-egress' | grep -q 'app.kubernetes.io/component: start' \
		|| { echo "chart-check FAILED: the Zitadel egress policy reaches past the server to Zitadel's own jobs" >&2; exit 1; }; \
	role="$$(echo "$$rendered" | awk '/^kind: Role$$/{r=1} r&&/^---/{r=0} r' | grep -A14 'name: check-meridian-runtime-identity-setup' )"; \
	echo "$$role" | grep -q 'resourceNames: \["check-meridian-runtime-dashboard-oidc", "check-meridian-runtime-group-hook"\]' \
		|| { echo "chart-check FAILED: the setup Job's Role is not limited to its two Secrets" >&2; exit 1; }; \
	echo "$$role" | grep -qE 'verbs:.*(create|list|watch|delete|\*)' \
		&& { echo "chart-check FAILED: the setup Job may do more than read and update its two Secrets" >&2; exit 1; }; \
	for refused in "zitadel.image.tag=" "zitadel.login.image.tag=v4.17.4" "zitadel.image.tag=v4.15.3" \
		"zitadel.zitadel.configmapConfig.ExternalDomain=" "dashboard.enabled=false" "identity.bundled.egress.allowCidrs=null" \
		"identity.bundled.ldap.enabled=true"; do \
		if $(HELM) template check deploy/chart $(CHART_VALUES) $$bundled --set $$refused >/dev/null 2>&1; then \
			echo "chart-check FAILED: the bundled Zitadel rendered with $$refused" >&2; exit 1; \
		fi; \
	done
	@$(HELM) template check deploy/chart $(CHART_VALUES) 2>/dev/null \
		| awk '/^kind: Job$$/{j=1} j&&/helm.sh\/hook/{print} /^---/{j=0}' | grep -q 'pre-install\|pre-upgrade\|post-install' \
		&& { echo "chart-check FAILED: a Job runs as a Helm hook. A hook must finish before the dashboard exists, and on a fresh install the wizard is what configures the database it would wait for" >&2; exit 1; }; \
	echo "chart-check OK: four components, the dashboard and the bundled Zitadel, the key on the conductor alone, both key paths, refusals, migrations, no pinned uid, and a plugin held to its side of the pod"

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
	@$(PY) tools/nats_permissions.py --with-dev-users --out deploy/nats/dev.conf >/dev/null
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
	@$(PY) tools/nats_permissions.py --with-dev-users --out deploy/nats/dev.conf >/dev/null
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

# The dashboard's sign-in and access, against a real Zitadel.
#
# spec/deployment-dashboard-and-access, Verification, for the parts that exist:
# a Zitadel-native person and an LDAP person brokered by Zitadel sign in; the
# groups reach the dashboard through the group hook; a claim makes the first
# deployment admin once; a group removed at the directory is gone at the next
# sign-in; a Zitadel session reused without the directory is refused on
# auth_time; the hook refuses what Zitadel did not sign; a restarted Zitadel
# still signs people in; a callback from another browser is refused.
#
# Everything runs on the compose network with no host port, from an empty
# project, and is taken down with its volumes whatever the outcome. The
# platform is a stand-in answering the one call a claim makes. Two steps are
# here rather than in the runner, because they need Docker: changing the
# directory, and restarting Zitadel.
E2E := MERIDIAN_DEPLOYMENT_ID=DEP-e2e MERIDIAN_PLATFORM_ADDRESS=http://fake-platform:8000 \
	MERIDIAN_CONFIG_DATABASE_URL=postgres://meridian:meridian@core-postgres:5432/meridian \
	MERIDIAN_DASHBOARD_URL=http://dashboard:8080 MERIDIAN_OIDC_ISSUER=http://zitadel:8080 \
	$(COMPOSE) --profile e2e
LDAP_ADMIN := -x -H ldap://localhost:1389 -D cn=admin,dc=example,dc=org -w ldap-admin-dev-only

e2e-dashboard: network
	@$(PY) tools/nats_permissions.py --with-dev-users --out deploy/nats/dev.conf >/dev/null
	@$(E2E) down -v >/dev/null 2>&1; started=$$(date +%s); \
	step() { echo "e2e-dashboard: $$1"; echo "== $$1" >>.e2e-dashboard.log; }; \
	boot() { $(E2E) run --rm -T --no-deps --entrypoint cat zitadel-bootstrap /bootstrap/$$1 2>>.e2e-dashboard.log; }; \
	bob() { $(E2E) exec -T ldap ldapsearch -LLL $(LDAP_ADMIN) -b ou=people,dc=example,dc=org '(uid=bob)' memberOf; }; \
	: >.e2e-dashboard.log; \
	{ step "building the runtime image" \
	  && $(E2E) build dashboard conductor group-hook zitadel-setup >>.e2e-dashboard.log 2>&1 \
	  && step "starting Postgres, the broker, Zitadel, LDAP, the group hook and the stand-in platform" \
	  && $(E2E) up -d postgres nats zitadel ldap group-hook fake-platform >>.e2e-dashboard.log 2>&1 \
	  && step "loading the directory, with memberOf" \
	  && $(E2E) exec -T ldap sh -c 'for i in $$(seq 1 60); do ldapsearch $(LDAP_ADMIN) -b "" -s base >/dev/null 2>&1 && exit 0; sleep 1; done; exit 1' \
	  && $(E2E) exec -T ldap ldapmodify -Q -Y EXTERNAL -H ldapi:/// <e2e/dashboard/ldap/01-memberof.ldif >>.e2e-dashboard.log 2>&1 \
	  && $(E2E) exec -T ldap ldapadd $(LDAP_ADMIN) <e2e/dashboard/ldap/02-tree.ldif >>.e2e-dashboard.log 2>&1 \
	  && step "applying the configuration store's schema" \
	  && $(E2E) run --rm -T conductor meridian-conductor migrate >>.e2e-dashboard.log 2>&1 \
	  && step "setting up Zitadel, as the chart's setup Job does" \
	  && $(E2E) run --rm -T zitadel-setup >>.e2e-dashboard.log 2>&1 \
	  && step "setting the same up again, which changes nothing" \
	  && first=$$(boot client-id)$$(boot intent-signing-key) \
	  && $(E2E) run --rm -T zitadel-setup >>.e2e-dashboard.log 2>&1 \
	  && [ "$$first" = "$$(boot client-id)$$(boot intent-signing-key)" ] \
	  && step "making the test's people" \
	  && $(E2E) run --rm -T zitadel-bootstrap >>.e2e-dashboard.log 2>&1 \
	  && step "Zitadel's database in the deployment's Postgres: $$($(E2E) exec -T postgres psql -U meridian -d meridian -Atc \
	     "select datname || ' owned by ' || pg_get_userbyid(datdba) || ', login role ' || (select rolname from pg_roles where rolname = 'zitadel' and rolcanlogin and not rolsuper) from pg_database where datname = 'zitadel'")" \
	  && client_id=$$(boot client-id) && project_id=$$(boot project-id) \
	  && step "starting the conductor and the dashboard (client $$client_id)" \
	  && MERIDIAN_OIDC_CLIENT_ID=$$client_id MERIDIAN_OIDC_TRUSTED_AUDIENCES=$$project_id \
	     $(E2E) up -d conductor dashboard >>.e2e-dashboard.log 2>&1; } \
	  || { echo "e2e-dashboard FAILED while setting up. The last 40 lines, and the whole of it in .e2e-dashboard.log:" >&2; \
	       tail -40 .e2e-dashboard.log >&2; $(E2E) down -v >/dev/null 2>&1; exit 1; }; \
	step "signing in: A, B, C, D (before), E, F, H"; \
	$(E2E) run --rm -T e2e-runner main >>.e2e-dashboard.log 2>&1; \
	step "removing bob from ldap-group-b at the directory"; \
	{ echo "before:"; bob; $(E2E) exec -T ldap ldapmodify $(LDAP_ADMIN) <e2e/dashboard/ldap/03-remove-bob-from-b.ldif; \
	  echo "after:"; bob; } >>.e2e-dashboard.log 2>&1; \
	step "signing in again: D (after)"; \
	$(E2E) run --rm -T e2e-runner after-removal >>.e2e-dashboard.log 2>&1; \
	step "restarting Zitadel, and not its database"; \
	$(E2E) restart zitadel >>.e2e-dashboard.log 2>&1; \
	step "signing in after the restart: G"; \
	$(E2E) run --rm -T e2e-runner after-restart >>.e2e-dashboard.log 2>&1; \
	$(E2E) logs --no-color zitadel group-hook dashboard conductor fake-platform >.e2e-dashboard.services.log 2>&1; \
	$(E2E) run --rm -T e2e-runner report 2>>.e2e-dashboard.log | tee -a .e2e-dashboard.log; status=$${PIPESTATUS[0]}; \
	$(E2E) down -v >/dev/null 2>&1; \
	echo "e2e-dashboard: $$(( $$(date +%s) - started ))s; the run in .e2e-dashboard.log, the services' logs in .e2e-dashboard.services.log"; \
	if [ $$status -ne 0 ]; then echo "e2e-dashboard FAILED" >&2; exit 1; fi
	@echo "e2e-dashboard OK: sign-in, groups, claim, freshness and the hook's refusals, against a real Zitadel"

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

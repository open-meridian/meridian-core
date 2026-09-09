SHELL := /bin/bash
PY    := python3

.PHONY: help ci-local ci-local-deep install-hooks ci-mirror-check

help:
	@echo "  make ci-local       run every gate (the pre-push gate, and what CI mirrors)"
	@echo "  make install-hooks  point git at hooks/ so push fires ci-local"

# Local green is the completion signal; CI is confirmation.
# Every job in .github/workflows must be reachable from here.
ci-local: ci-mirror-check
	@echo
	@echo "ci-local: GREEN"

ci-local-deep: ci-local

ci-mirror-check:
	@$(PY) tools/ci_mirror_check.py --repo-root .

install-hooks:
	@git config core.hooksPath hooks
	@echo "hooks installed: git push now runs 'make ci-local' first"

.PHONY: build test test-all test-minikube test-scripts test-postgres crds helm-crds check fmt clippy \
       docker-build docker-push docker-load install minikube-setup state-machine \
       release-patch release-minor release-major

# ── Configuration ─────────────────────────────────────────────────────────────

IMG ?= registry.bemade.org/bemade/odoo-operator:local-dev
CONTAINER_TOOL ?= docker
CRD_OUT_DIR := crds
HELM_CHART_DIR := charts/odoo-operator/templates/crds

# ── Build ─────────────────────────────────────────────────────────────────────

build:
	cargo build

build-release:
	cargo build --release

check:
	cargo check

fmt:
	cargo fmt --check

clippy:
	cargo clippy -- -D warnings

# ── Test ──────────────────────────────────────────────────────────────────────

## Unit + pure logic tests (no cluster needed)
test:
	cargo test

## All tests, including the end-to-end smoke on the local minikube test
## cluster (requires docker and `make minikube-setup` first)
test-all: test test-scripts test-postgres test-minikube

## End-to-end smoke (staging refreshes, neutralize, Mailpit) on the minikube
## test cluster; see testing/minikube/README.md
test-minikube:
	testing/minikube/smoke.sh

## Shell-script contract tests for scripts/* (requires docker, zip)
test-scripts:
	@for t in scripts/tests/test-*.sh; do \
		echo "=== $$t ==="; \
		bash "$$t" || exit $$?; \
	done

## Docker-backed PostgreSQL integration tests for PgPostgresManager (requires docker)
test-postgres:
	cargo test --test postgres_harness -- --test-threads=1

# ── Docker ────────────────────────────────────────────────────────────────────

docker-build: ## Build docker image with the operator.
	$(CONTAINER_TOOL) build -t $(IMG) .

docker-push: ## Push docker image.
	$(CONTAINER_TOOL) push $(IMG)

# ── Local minikube test cluster (testing/minikube/README.md) ─────────────────
# These go through testing/minikube/*.sh, which only ever use the profile's
# dedicated kubeconfig and refuse any other cluster, so they can never act on
# the default kubeconfig context.

REV ?= HEAD

## Create the minikube test cluster with the latest released operator.
minikube-setup:
	testing/minikube/setup.sh all

## Build REV (default HEAD, committed tree only) and load it into minikube.
docker-load:
	testing/minikube/upgrade-local.sh $(REV) --build-only

## Build REV (default HEAD, committed tree only) and helm-upgrade the minikube
## test cluster's operator to it, with REV's own chart and CRDs.
install:
	testing/minikube/upgrade-local.sh $(REV)

# ── CRD Generation ───────────────────────────────────────────────────────────

## Generate raw CRDs from Rust types into $(CRD_OUT_DIR)/
crds:
	cargo run --bin crdgen -- --out-dir $(CRD_OUT_DIR)

# helm-crd-inject:
#   1. Adds helm.sh/resource-policy: keep annotation.
#   2. Injects a Helm template include for the v1 legacy shim (served:false,
#      storage:false, open schema) immediately after "  versions:" so that
#      clusters previously running the Kopf Python operator (which stored objects
#      as bemade.org/v1) can upgrade without manual kubectl patching.
# $(1) = source CRD yaml, $(2) = destination path, $(3) = include name
define helm-crd-inject
	sed \
	  -e '/^metadata:$$/a\  annotations:\n    helm.sh/resource-policy: keep' \
	  -e 's|^  versions:$$|  versions:\n{{- include "$(3)" . \| nindent 2 }}|' \
	  $(1) > $(2)
endef

# helm-crd-inject-new is used for CRDs that never existed in the Kopf
# operator — no legacy v1 shim, just the keep annotation.
# $(1) = source, $(2) = destination
define helm-crd-inject-new
	sed \
	  -e '/^metadata:$$/a\  annotations:\n    helm.sh/resource-policy: keep' \
	  $(1) > $(2)
endef

## Generate CRDs from Rust types and sync into the Helm chart with legacy v1 shim.
helm-crds: crds
	@mkdir -p "$(HELM_CHART_DIR)"
	$(call helm-crd-inject,$(CRD_OUT_DIR)/odoo-crd.yaml,$(HELM_CHART_DIR)/odoo-crd.yaml,odoo-operator.v1LegacyVersionWithScale)
	$(call helm-crd-inject,$(CRD_OUT_DIR)/initjob-crd.yaml,$(HELM_CHART_DIR)/initjob-crd.yaml,odoo-operator.v1LegacyVersion)
	$(call helm-crd-inject,$(CRD_OUT_DIR)/backupjob-crd.yaml,$(HELM_CHART_DIR)/backupjob-crd.yaml,odoo-operator.v1LegacyVersion)
	$(call helm-crd-inject,$(CRD_OUT_DIR)/restorejob-crd.yaml,$(HELM_CHART_DIR)/restorejob-crd.yaml,odoo-operator.v1LegacyVersion)
	$(call helm-crd-inject,$(CRD_OUT_DIR)/upgradejob-crd.yaml,$(HELM_CHART_DIR)/upgradejob-crd.yaml,odoo-operator.v1LegacyVersion)
	$(call helm-crd-inject-new,$(CRD_OUT_DIR)/stagingrefreshjob-crd.yaml,$(HELM_CHART_DIR)/stagingrefreshjob-crd.yaml)
	@echo "CRDs synced to $(HELM_CHART_DIR)"

## Regenerate STATE_MACHINE.md from the TRANSITIONS table in code.
state-machine:
	cargo run --bin statemachine_diagram -- --out STATE_MACHINE.md

# ── Release ───────────────────────────────────────────────────────────────────

## Cut a release: make release-patch / release-minor / release-major
release-patch release-minor release-major:
	@command -v cargo-bump >/dev/null 2>&1 || \
	  (echo "ERROR: cargo-bump not installed — run: cargo install cargo-bump" && exit 1)
	@git diff --quiet && git diff --cached --quiet || \
	  (echo "ERROR: working tree is dirty — commit or stash changes first" && exit 1)
	cargo bump $(subst release-,,$@)
	$(eval VERSION := $(shell grep '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/'))
	sed -i 's/^version:.*/version: $(VERSION)/' charts/odoo-operator/Chart.yaml
	sed -i 's/^appVersion:.*/appVersion: "$(VERSION)"/' charts/odoo-operator/Chart.yaml
	cargo generate-lockfile
	git add Cargo.toml Cargo.lock charts/odoo-operator/Chart.yaml
	git commit -m "chore: bump version to $(VERSION)"
	git tag "v$(VERSION)"
	git push origin HEAD "v$(VERSION)"
	@echo "Done — CI will build and publish v$(VERSION)."

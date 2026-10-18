# MemKeys operator (M10b / ISI-6085). Mirrors the kubebuilder Makefile, trimmed
# to the targets CI and contributors actually use.

# Pin the Kubernetes version envtest downloads binaries for.
ENVTEST_K8S_VERSION ?= 1.31.0

# Image to build/push for the operator manager.
IMG ?= ghcr.io/memkeys/memkeys-operator:latest

LOCALBIN ?= $(shell pwd)/bin
CONTROLLER_GEN ?= $(LOCALBIN)/controller-gen
SETUP_ENVTEST ?= $(LOCALBIN)/setup-envtest

CONTROLLER_GEN_VERSION ?= v0.16.5
# setup-envtest is versioned with controller-runtime.
SETUP_ENVTEST_VERSION ?= release-0.19

.PHONY: all
all: build

$(LOCALBIN):
	mkdir -p $(LOCALBIN)

.PHONY: controller-gen
controller-gen: $(LOCALBIN)
	test -s $(CONTROLLER_GEN) || GOBIN=$(LOCALBIN) go install sigs.k8s.io/controller-tools/cmd/controller-gen@$(CONTROLLER_GEN_VERSION)

.PHONY: setup-envtest
setup-envtest: $(LOCALBIN)
	test -s $(SETUP_ENVTEST) || GOBIN=$(LOCALBIN) go install sigs.k8s.io/controller-runtime/tools/setup-envtest@$(SETUP_ENVTEST_VERSION)

.PHONY: generate
generate: controller-gen ## Generate DeepCopy methods.
	$(CONTROLLER_GEN) object:headerFile=/dev/null paths=./api/...

.PHONY: manifests
manifests: controller-gen ## Generate CRD + RBAC manifests.
	$(CONTROLLER_GEN) crd paths=./api/... output:crd:artifacts:config=config/crd/bases
	$(CONTROLLER_GEN) rbac:roleName=memkeys-operator-role paths=./internal/... output:rbac:artifacts:config=config/rbac

.PHONY: fmt
fmt:
	go fmt ./...

.PHONY: vet
vet:
	go vet ./...

.PHONY: build
build: generate fmt vet ## Build the manager binary.
	go build -o $(LOCALBIN)/manager ./cmd

.PHONY: test
test: manifests generate fmt vet setup-envtest ## Run unit + envtest integration tests.
	KUBEBUILDER_ASSETS="$(shell $(SETUP_ENVTEST) use $(ENVTEST_K8S_VERSION) --bin-dir $(LOCALBIN)/envtest -p path)" \
		go test ./... -coverprofile cover.out

.PHONY: docker-build
docker-build: ## Build the operator container image.
	docker build -t $(IMG) .

.PHONY: verify
verify: manifests generate fmt ## Fail if generated artifacts are stale.
	git diff --exit-code

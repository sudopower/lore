VERSION ?= dev
NS ?= lore
KCTX = --context kind-factory
GEN = deploy/k8s/overlays/generated
DATABASE_URL ?= postgres://lore:lore@localhost:5434/lore
export DATABASE_URL

.PHONY: db-up run test lint check image kind-up kind-load kind-secret kind-deploy kind-rollback kind-logs kind-delete

db-up:
	docker compose up -d --wait db

run:
	LORE_EMBEDDER=hash LORE_LLM=fake LORE_ADMIN_TOKEN=dev-admin cargo run -- serve

test:
	cargo test --all-targets

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings

check: lint test

image:
	docker build --build-arg VERSION=$(VERSION) -t lore:$(VERSION) .

# Kind deployment. Every kubectl call passes $(KCTX): the kubeconfig also holds other
# clusters, and `kind create cluster` switches the current context.
# NS must be lore (the releaser's) or lore-dev-* (a builder's).
CHECK_NS = $(if $(filter lore lore-dev-%,$(NS)),,$(error NS must be lore or lore-dev-*, got '$(NS)'))

kind-up:
	kind get clusters | grep -qx factory || kind create cluster --name factory

kind-load:
	kind load docker-image lore:$(VERSION) --name factory

kind-secret:
	@: $(CHECK_NS)
	kubectl $(KCTX) create namespace $(NS) --dry-run=client -o yaml | kubectl $(KCTX) apply -f -
	@kubectl $(KCTX) -n $(NS) get secret lore-db >/dev/null 2>&1 \
		|| kubectl $(KCTX) -n $(NS) create secret generic lore-db --from-literal=POSTGRES_PASSWORD=$$(openssl rand -hex 16)
	@if ! kubectl $(KCTX) -n $(NS) get secret lore-env >/dev/null 2>&1; then \
		pw=$$(kubectl $(KCTX) -n $(NS) get secret lore-db -o jsonpath='{.data.POSTGRES_PASSWORD}' | base64 -d); \
		kubectl $(KCTX) -n $(NS) create secret generic lore-env \
			--from-literal=LORE_ADMIN_TOKEN=$$(openssl rand -hex 24) \
			--from-literal=LORE_DATABASE_URL="postgres://lore:$$pw@lore-db:5432/lore"; \
	fi

kind-deploy: kind-secret
	mkdir -p $(GEN)
	printf 'apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - ../../base\nnamespace: $(NS)\nimages:\n  - name: lore\n    newName: lore\n    newTag: "$(VERSION)"\n' > $(GEN)/kustomization.yaml
	@img=$$(kubectl $(KCTX) -n $(NS) get deploy lore -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true); \
	case "$$img" in \
		*:*) tag=$${img##*:}; \
			if [ "$$tag" != "$(VERSION)" ]; then echo "$$tag" > $(GEN)/.previous-$(NS); echo "previous tag $$tag saved to $(GEN)/.previous-$(NS)"; fi ;; \
	esac
	kubectl $(KCTX) apply -k $(GEN)
	kubectl $(KCTX) -n $(NS) rollout status deploy/lore --timeout=300s

kind-rollback:
	@: $(CHECK_NS)
	@prev=$$(cat $(GEN)/.previous-$(NS) 2>/dev/null || true); \
	if [ -n "$$prev" ]; then \
		echo "rolling back $(NS) to $$prev"; \
		make kind-deploy NS=$(NS) VERSION=$$prev; \
	else \
		kubectl $(KCTX) -n $(NS) rollout undo deploy/lore \
			&& kubectl $(KCTX) -n $(NS) rollout status deploy/lore --timeout=300s; \
	fi

kind-logs:
	@: $(CHECK_NS)
	kubectl $(KCTX) -n $(NS) logs deploy/lore --tail=200

# Dev namespaces only: lore (the releaser's) is refused, and so is anything not named lore-dev-*.
kind-delete:
	@case "$(NS)" in lore-dev-?*) ;; *) echo "kind-delete is for dev namespaces only"; exit 1 ;; esac
	kubectl $(KCTX) delete namespace $(NS) --wait=false

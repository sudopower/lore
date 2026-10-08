VERSION ?= dev
DATABASE_URL ?= postgres://lore:lore@localhost:5434/lore
export DATABASE_URL

.PHONY: db-up run test lint check image

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

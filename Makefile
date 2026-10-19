.PHONY: help deps build release test test-one fmt lint check clean serve apply

help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  %-12s %s\n",$$1,$$2}'

deps: ## fetch deps
	cargo fetch

build: ## debug build
	cargo build

release: ## release build
	cargo build --release

test: ## integration tests (needs a Docker API; docs/develop-without-docker.md)
	cargo test

test-one: ## run one test: make test-one T=name
	cargo test $(T)

fmt: ## format
	cargo fmt

lint: ## clippy
	cargo clippy --all-targets -- -D warnings

check: fmt lint test ## fmt+lint+test

clean: ## wipe target
	cargo clean

serve: ## start daemon
	cargo run -- serve

apply: ## apply example: make apply F=examples/nginx.yaml
	cargo run -- apply -f $(F)

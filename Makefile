.PHONY: bootstrap build run fmt clippy test

bootstrap:
	bash scripts/bootstrap.sh

build:
	cargo build

run:
	cargo run

fmt:
	cargo fmt

clippy:
	cargo clippy -- -D warnings

test:
	cargo test

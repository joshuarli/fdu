NAME := fdu

.PHONY: build test lint release install

build:
	cargo build --locked

test:
	cargo test --locked --workspace
	cargo test --locked -p fdu --no-default-features

lint:
	cargo clippy --fix --allow-dirty --all-targets --all-features -- --deny warnings

release:
	cargo build --locked --release

install: release
	cp target/release/$(NAME) ~/usr/bin/$(NAME)
	@if test "$$(uname -s)" = Darwin; then \
		codesign -fs - ~/usr/bin/$(NAME); \
	fi

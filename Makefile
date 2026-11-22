.DEFAULT_GOAL := build

.PHONY: install-dev build clean check test

CARGO := cargo

install-dev:
	$(CARGO) build
	du -h target/debug/lattice
	test -d $(HOME)/.local/bin && install -m 755 target/debug/lattice $(HOME)/.local/bin/lattice && echo "Debug Binary installed to $(HOME)/.local/bin/" || true

build:
	$(CARGO) build --release
	du -h target/release/lattice
	test -d $(HOME)/.local/bin && install -m 755 target/release/lattice $(HOME)/.local/bin/lattice && echo "Release Binary installed to $(HOME)/.local/bin/" || true

check:
	$(CARGO) check

test:
	$(CARGO) build
	$(CARGO) test

clean:
	$(CARGO) clean

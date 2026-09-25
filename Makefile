.PHONY: all build test live lint fmt check diagrams

all: build

build:
	cargo build --workspace

test:
	cargo test --workspace

# Needs the network. Boots containers against the engine SANDUK_RUNTIME names, or this platform's
# default, and relays one unbilled call to api.anthropic.com over TLS.
live:
	cargo test -p sanduk-container --test live -- --ignored --test-threads 1
	cargo test -p sanduk --test live_tls -- --ignored

# Matches what CI runs. `make fmt` applies what `lint` only reports.
lint:
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt

check: lint test

# Every docs/media/*.d2 to an SVG beside it. TALA is d2's layout engine for architecture diagrams.
diagrams:
	@for f in docs/media/*.d2; do d2 --layout=tala "$$f" "$${f%.d2}.svg" || exit 1; done

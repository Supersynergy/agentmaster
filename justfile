# agentmaster — repeatable tasks. Stable verbs only.

default: check

# install/reshim toolchain (rust components)
setup:
    rustup component add clippy rustfmt

# tool availability + runtime health
doctor:
    cargo run -q -- doctor

# run the TUI
dev:
    cargo run

# unit tests
test:
    cargo test

# lint (clippy, warnings = fail)
lint:
    cargo clippy --all-targets -- -D warnings

# format
fmt:
    cargo fmt

# format check + lint + build (fast gate)
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo build

# full gate: doctor + check + test + release build
ci: doctor check test
    cargo build --release

# release binary
build:
    cargo build --release

# pre-pr: ci + security
pre-pr: ci
    cargo audit || true

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

build:
    cargo build --workspace

test:
    cargo test --workspace

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# Run four demo backends and the gateway locally. Ctrl-C stops everything.
dev:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --workspace
    trap 'kill 0' EXIT
    ./target/debug/demo-service --name users-1     --bind 127.0.0.1:9001 &
    ./target/debug/demo-service --name users-2     --bind 127.0.0.1:9002 &
    ./target/debug/demo-service --name orders-1    --bind 127.0.0.1:9003 &
    ./target/debug/demo-service --name analytics-1 --bind 127.0.0.1:9004 &
    ./target/debug/rustgate --config config/dev.yaml

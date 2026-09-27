# RustGate

A production-style API gateway / reverse proxy in Rust, built as a portfolio project. The
goal is a codebase the author can defend in depth in a backend/systems interview:
concurrency, networking, security, observability, fault tolerance, performance.

**Status:** Phase 1 done. Next is Phase 2, starting with core metrics (`/metrics`), then
load balancing, health checks, overload protection, readiness during drain, and the baseline
benchmark. The full plan, the benchmarking method, the config-source rule and every decision
made since the original design are in [docs/roadmap.md](docs/roadmap.md).

## Commands

`cargo` is not on the default PATH in the agent shell: `export PATH="$HOME/.cargo/bin:$PATH"`.

```sh
just test    # cargo test --workspace (unit + integration)
just lint    # cargo fmt --check + clippy -D warnings (pedantic)
just fmt
just dev     # 4 demo backends on :9001-9004 + gateway on :8080, admin on :9901
```

Before calling any change done, run `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
All three must pass. CI (`.github/workflows/ci.yml`) runs the same checks with `--locked`.

## Layout

- `crates/rustgate`: library plus a thin binary. Integration tests start a real `Gateway`
  in-process on port 0.
  - `config/`: serde schema (what users write). `deny_unknown_fields` everywhere.
  - `snapshot.rs`: compiled, immutable runtime config behind `ArcSwap`. Validation happens
    in `Snapshot::compile`, so an invalid config never reaches traffic.
  - `routing/`: host → segment-trie longest-prefix router, property-tested against a linear
    scan.
  - `upstream/`: `EndpointRegistry`, clusters, endpoints, the pooled hyper client.
  - `proxy/`: the request pipeline, header handling, and the error → response mapping.
  - `server.rs`: accept loop, h1/h2 via `auto::Builder`, graceful drain.
  - `admin/`: axum admin API on a separate listener. `telemetry/`: tracing setup.
- `crates/rustgate/tests/proxy.rs`: end-to-end tests against in-process axum upstreams.
- `crates/demo-service`: a configurable backend for demos (`?delay_ms=`, `?status=`).
- `config/dev.yaml`: local config used by `just dev`.

## Architecture invariants

- **Data plane never awaits the control plane.** They share only `ArcSwap` snapshots and
  atomics. No locks on the request path. `Mutex` is fine in control-plane code (e.g. the
  registry, touched only while compiling a snapshot).
- **Per-endpoint runtime state lives on `Endpoint`, interned in `EndpointRegistry`,** never
  in the snapshot. That way it survives config reloads and is shared across clusters.
- **Pipeline is fixed phases, not a user-assembled Tower stack.** The order is part of the
  security model (validate → route → auth → rate limit → concurrency → pick endpoint →
  breaker → forward → retry). Tower/axum only on the admin API.
- **Bodies are never buffered.** Stream `Incoming` through as `ProxyBody`. Any feature that
  needs to buffer (retries) must be bounded and explicit.
- **Every failure becomes a response** with a stable JSON `error` code (`proxy/error.rs`).
  `Proxy::handle` is infallible.
- **Strip hop-by-hop headers in both directions.** Don't rely on hyper's HTTP/2 stripping.

## Conventions

- **No premature optimization.** Build the simple, correct version first. Mark suspected hot
  spots with `// PERF:` and a reason. Change them only with profiling data (Phase 9),
  recording before/after numbers.
- Deferred work is marked `TODO(phase N): ...` so it's greppable per phase.
- Comments explain *why* (tradeoffs, protocol rules, RFC references), not what.
- Use traits only where they pay for themselves. Avoid enterprise-style layering.
- Errors: `thiserror` in the library, `anyhow` only in `main`. No `unwrap` outside tests.
- Every phase ends with tests proving its "Done when" criterion from the roadmap. Check new
  tests catch the bug they target: break the code briefly and confirm they fail.
- **Never invent benchmark numbers.** The README only gets measured results, with the
  scripts to reproduce them in `bench/`, following the method in the roadmap.
- **The README describes only what is implemented.** Update its "What works today" list at
  the end of each phase, alongside this file.
- Each phase adds metrics for the behavior it introduces (auth failures, rate-limited
  requests, retries, breaker transitions).

## Working with the author

- Work incrementally. Don't dump huge amounts of code at once.
- For each major component: explain the design, implement it, show how to run and test it,
  explain the Rust concepts involved (ownership, `Arc`, atomics, cancellation, backpressure),
  and state the performance implications.
- Push back when a requirement is technically wrong, and explain why.

## Keeping this file current

Update this file in the same change whenever the layout, commands, invariants, or current
phase change. Record new design decisions in `docs/roadmap.md`.

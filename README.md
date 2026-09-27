# RustGate

An API gateway / reverse proxy written in Rust on Tokio and hyper, built to explore what
production gateways like Envoy and Pingora do internally. For real deployments, use one of
those. This project exists to understand and measure the design choices, not to replace them.

> **Status: Phase 1 of 10.** What works today is listed below; everything else is planned in
> [docs/roadmap.md](docs/roadmap.md). This README only describes implemented behavior and
> will only ever quote benchmark numbers produced by the scripts in this repo.

## What works today

- **Reverse proxying over HTTP/1.1 and HTTP/2 (h2c)** on one port, with pooled upstream
  connections.
- **Host + path-prefix routing:** a segment trie with longest-prefix match. Host-specific
  routes are exclusive, so there's no accidental fallback to a catch-all.
- **Streaming bodies in both directions.** Nothing is buffered, and backpressure flows end
  to end through hyper.
- **Correct proxy header handling:** hop-by-hop headers are stripped in both directions,
  including ones named in `Connection`. The gateway adds `X-Forwarded-For/-Proto/-Host`,
  forwards `te: trailers` for gRPC, and blocks paths with `.`/`..` segments.
- **Request IDs:** a client-supplied `x-request-id` is kept only if it's safe, otherwise a
  UUIDv7 is generated. The ID is propagated upstream, echoed to the client, and appears on
  every log line.
- **Stable JSON errors** that tell gateway rejections (`400`/`404`/`503`) apart from
  upstream failures (`502`).
- **Graceful shutdown:** the gateway stops accepting, drains in-flight requests, and aborts
  at a deadline.
- **Separate admin listener** (localhost) with `GET /admin/health`.

## Design in brief

```
            ┌──────────────── data plane (hot path, lock-free) ────────────────┐
clients ──▶ │ accept → route (ArcSwap snapshot) → pick endpoint → stream proxy │ ──▶ upstreams
            └──────────────────────────────▲───────────────────────────────────┘
                                           │ atomic pointer swap
            ┌──────────────── control plane (background) ──────────────────────┐
  admin ──▶ │ load + validate config → compile snapshot   endpoint registry    │
            └──────────────────────────────────────────────────────────────────┘
```

- The data plane never waits on the control plane. They share immutable snapshots
  (`ArcSwap`) and atomics only.
- Per-endpoint state (health, breaker, in-flight counts) lives in a registry that outlives
  config snapshots, so config reloads don't reset it.
- The request pipeline is a fixed sequence of phases rather than a pluggable middleware
  stack, because the order (auth before rate limiting, and so on) is part of the security
  model.

The reasoning behind these decisions is in [docs/roadmap.md](docs/roadmap.md).

## Running it

Requires Rust (pinned in `rust-toolchain.toml`) and [`just`](https://github.com/casey/just).

```sh
just dev     # 4 demo backends + gateway on :8080, admin on :9901
curl -i localhost:8080/users/42
curl -i -H 'Host: orders.localhost' localhost:8080/anything
curl -s localhost:9901/admin/health

just test    # unit + end-to-end tests
just lint    # rustfmt + clippy (pedantic, warnings as errors)
```

The demo backends accept `?delay_ms=N` and `?status=CODE` to simulate slow or failing
services.

## Benchmarks

Not measured yet. The first baseline arrives with Phase 2. The methodology (open-loop load
at a fixed rate, pinned cores, published hardware, comparison against nginx, Envoy and a
Node.js proxy) is described in [docs/roadmap.md](docs/roadmap.md#benchmarking).

## License

MIT OR Apache-2.0

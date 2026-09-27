# Roadmap

Each phase builds the simple, correct version first. Suspected hot spots get a `// PERF:`
note and stay as they are until Phase 9 profiling shows they matter.

Every phase ends in a presentable state: tests for its "Done when" criterion, CI green, and
the README updated with what now works (and measured numbers once they exist). If the
project stops after any phase, the repo still tells a complete, honest story.

| Phase | Deliverable | Done when |
|---|---|---|
| 1 ✅ | Hyper proxy, host + prefix router, pooled client, admin health, demo service, request IDs + structured logs, graceful drain plumbing, endpoint registry, CI (fmt, clippy, test) | Integration tests: routing, streaming, hop-by-hop headers removed, 502 on refused connection, drain (`crates/rustgate/tests/proxy.rs`) |
| 2 | Core metrics (`/metrics`: requests by route/status, latency histogram, in-flight gauge); round-robin / least-requests; active health checks; overload protection (max connections, max in-flight → 503); readiness that fails during drain; baseline benchmark (see [Benchmarking](#benchmarking)) | Endpoint removed and restored while under load; zero dropped requests during drain; first measured numbers in the README |
| 3 | `RequestContext` threaded through pipeline phases; JWT/OIDC resource server, JWKS cache; mock IdP demo service; auth-failure metrics | Expired, bad-signature, wrong-aud and alg-confusion tests pass |
| 4 | Local token bucket, Redis GCRA, fallback policy; rate-limit metrics | proptest bucket invariants; Redis-outage test |
| 5 | Timeouts (connect / per-try / request / idle), replayable body, retries plus budget, per-endpoint breaker; retry and breaker-transition metrics | Deterministic breaker transition tests; POST not retried; transitions visible on `/metrics` |
| 6 | TLS termination (rustls, ALPN h2/h1, SNI), HTTP/2 to upstreams, gRPC passthrough, WebSocket upgrade, streaming idle timeouts | Echo test; gRPC unary + streaming call through the gateway (trailers intact); large streaming body with flat memory use |
| 7 | OTel tracing to Jaeger, Grafana dashboards, Docker Compose environment | One trace shows client → attempt 1 (failed) → attempt 2; `just up` brings up the full stack |
| 8 | Hot reload (file, then Redis), admin CRUD with versions, **admin API authentication**, TLS certificate reload | Reload under load: no errors, breaker state kept; admin writes without a valid token get 401 |
| 9 | Profiling (perf, flamegraph, allocation counts) and targeted fixes; comparative benchmarks | Each fix has before/after data |
| 10 | Chaos scenarios, `cargo audit` + Docker build in CI, README polish | Clean clone → `just up` → demo works |

## Benchmarking

The project's claim is that Rust suits this kind of infrastructure. Benchmarks must test
that claim, not just measure the gateway against itself.

**Scenarios**
1. Direct to backend (the floor).
2. Through RustGate, proxy only (routing included; every request is routed).
3. Scenario 2 plus one feature each: rate limiting, JWT validation, tracing. Each is
   measured when its phase lands, so its overhead is recorded at that point.
4. **Reference proxies** (Phase 9): the same topology through nginx and Envoy for a mature
   baseline, and a Node.js proxy for the Rust-vs-GC comparison.

**What to compare**
- Throughput and CPU per request.
- Tail latency (p99, p99.9) at a *fixed* request rate near saturation. This is where a
  garbage-collected runtime is expected to differ.
- Resident memory at 10k idle keep-alive connections and at 10k concurrent streams.

**Method**
- Use an open-loop load generator at a fixed rate (`wrk2`, or `oha --qps`). A closed-loop
  tool slows down with the server and hides tail latency (coordinated omission).
- Pin the load generator, gateway and backends to separate cores (`taskset`). Run on bare
  metal or report the virtualization. Publish CPU model, core count, kernel and versions.
- Warm up, then run multiple rounds and report the spread, not a single best run.
- Latency percentiles come from the load generator. Prometheus histogram quantiles are
  bucket estimates and are only used for dashboards.
- Scripts and raw output go in `bench/`. The README quotes only numbers produced by those
  scripts.

## Config sources: one writer

Once hot reload and admin CRUD both exist, there must be exactly one source of truth:

- **File mode** (default, single instance): the YAML file is authoritative and is watched for
  changes. The admin API is **read-only**. Writes return `409` pointing at the file.
- **Redis mode** (multi-instance): Redis holds the versioned config document and is the only
  writer. The file, if given, only seeds an empty Redis. Admin writes use compare-and-set on
  the version.

This prevents a file reload from silently reverting a route added through the API.

## Security scope

- The admin listener binds to localhost by default. From Phase 8 on, every admin endpoint
  except health/readiness needs a bearer token (config or env), compared in constant time.
  mTLS is an optional extension. `/metrics` stays unauthenticated but lives on the admin
  listener, never the data plane.
- `X-Forwarded-For` is extended only for trusted proxies (Phase 3), since rate-limit keys
  and IP lists depend on it.
- The demo auth service is a **mock OIDC identity provider**: discovery document, JWKS,
  and an endpoint that mints test tokens (valid, expired, wrong audience, wrong key). The
  gateway only validates tokens; it has no login flow.

## Decisions made after the original design

- **Endpoint state lives in an `EndpointRegistry`, not in the snapshot.** Snapshots are
  rebuilt on every reload, but health, breaker state and in-flight counts must survive one
  (Phase 8's "breaker state kept"). Endpoints are interned by normalized URL and held `Weak`,
  so a reload reuses them and a removed endpoint's state is dropped once nothing uses it.
  Two clusters listing one URL share one endpoint.
- **Shutdown is a `CancellationToken` plus hyper-util's `GracefulShutdown`**, built in Phase 1
  so that tests (and Phase 2's drain criteria) don't need a lifecycle refactor later.
  `Gateway::shutdown(deadline)` stops accepting, drains, and aborts at the deadline.
- **Metrics moved from Phase 7 to Phase 2**, with each later phase adding its own. The
  Phase 2 benchmark and the "observable breaker transitions" requirement both need them.
  Phase 7 keeps tracing, dashboards and the Compose stack. (The original plan listed basic
  metrics under Phase 1; they were not built there.)
- **CI moved from Phase 10 to Phase 1.** fmt, clippy and tests run on every push from the
  start. `cargo audit` and the Docker build join in Phase 10.
- **TLS termination had no phase**; it is now Phase 6, with ALPN and upstream HTTP/2.
- **`te: trailers` is forwarded; other `TE` values are stripped.** gRPC depends on it; this
  is the same rule Envoy follows.
- **Overload protection added to Phase 2.** Without a connection cap and an in-flight limit,
  a spike becomes unbounded memory and latency instead of fast 503s.
- **`RequestContext` arrives with auth (Phase 3)**, not before: until a phase produces data
  for a later one (principal → rate-limit key), passing locals is simpler.
- **Phase 5 note:** hyper's legacy client retries requests that failed on a reused pooled
  connection before being written (`retry_canceled_requests`). This is safe, but the retry
  budget must either count these retries or turn the behaviour off, so the numbers are honest.
- **Scope over breadth:** if time runs short, a polished Phase 1–7 beats a thin Phase 1–10.

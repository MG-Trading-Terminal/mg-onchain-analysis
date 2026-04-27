# Design 0011 — `crates/gateway`: HTTP + WebSocket Gateway

**Date:** 2026-04-21
**Status:** Draft
**Author:** architect agent
**Sprint:** 5 (P5-2)
**ADR refs:**
- ADR 0001 §D8 — three delivery modes (in-process crate, REST, WS streaming)
- ADR 0002 — Postgres-only storage
- ADR 0003 — self-sovereign infrastructure; no Auth0/Clerk/Cognito/Pusher/Ably
**Design refs:**
- `docs/designs/0001-crates-common-types.md` — `AnomalyEvent`, `Evidence`, `Severity`
- `docs/designs/0003-detector-trait.md` — `Detector` trait, on-demand invocation model
- `docs/designs/0010-scoring.md` — `TokenRiskReport`, `ScoringEngine`, streaming shape §12
- `crates/common/src/anomaly.rs` — wire types
- `crates/common/src/token.rs` — `TokenMeta`
- `crates/scoring/src/lib.rs` — `ScoringEngine::score()`
- `crates/storage/src/pg.rs` — `PgStore` query surface

---

## 1. Context and Scope

`crates/gateway` is the HTTP + WebSocket transport layer that exposes the detection and
scoring pipeline to three of the four consumers:

| Consumer | Mode | Primary endpoints |
|----------|------|-------------------|
| `bot-trader-2-0` | REST on-demand | `POST /v1/tokens/analyze` |
| `mg-custody` | REST cached reads | `GET /v1/tokens/{chain}/{mint}/risk` |
| Market maker | WebSocket streaming | `GET /v1/ws/stream` |
| Exchange | REST batch query + audit | `GET /v1/anomaly_events`, `POST /v1/tokens/analyze` |

The fourth consumer (`bot-trader-2-0` in-process crate mode) bypasses this crate entirely:
it links `crates/detectors` and `crates/scoring` directly. `crates/gateway` does not need to
support in-process operation — it is a service-mode concern only.

### What this design covers

- REST endpoint catalog and request/response shapes
- WebSocket subscription protocol (subscribe, push, heartbeat, backpressure, resume)
- Authentication model: self-signed JWT (Ed25519), Argon2id user store, scopes, mTLS flag
- Error shape: RFC 7807 problem details + full error taxonomy
- In-process scoring cache (`moka`) + invalidation via tokio broadcast
- Observability: structured tracing + Prometheus metrics
- Operational concerns: graceful shutdown, SIGHUP config reload, secret hygiene
- Configuration shape (`GatewayConfig`)
- Dependency recommendations
- Developer acceptance checklist

### What this design does NOT cover

- Implementation of `crates/detectors` or `crates/scoring` (complete, see 0003/0010)
- `crates/client-sdk` (Phase 4)
- Webhook delivery to mg-custody (Phase 6)
- Token revocation list for JWTs (Phase 6 — stateless JWTs are sufficient for MVP)
- OAuth2 client credentials grant (Phase 6)
- Distributed cache / Redis (Phase 6+)
- Integration code into `~/Projects/bot-trader-2-0` (bot team integrates independently)

---

## 2. Module Layout

```
crates/gateway/
  Cargo.toml
  src/
    lib.rs              -- Re-exports: GatewayConfig, run_gateway()
    router.rs           -- Axum router assembly: all routes + middleware stack
    handlers/
      analyze.rs        -- POST /v1/tokens/analyze
      risk.rs           -- GET /v1/tokens/{chain}/{mint}/risk
      events.rs         -- GET /v1/anomaly_events
      detectors.rs      -- GET /v1/detectors
      health.rs         -- GET /health
      metrics.rs        -- GET /metrics (Prometheus text)
      auth.rs           -- POST /v1/auth/token (JWT mint endpoint)
      jwks.rs           -- GET /v1/.well-known/jwks.json
    ws/
      mod.rs            -- WebSocket upgrade + connection lifecycle
      subscription.rs   -- Subscription filter: chain, tokens, detectors, severity_min
      dispatcher.rs     -- Fan-out from broadcast channel → per-subscriber send
      heartbeat.rs      -- Server ping / client PONG timeout
    auth/
      mod.rs            -- JWT middleware: extract + validate bearer token
      jwt.rs            -- JWT sign/verify (Ed25519 via ed25519-dalek)
      argon.rs          -- Argon2id hash + verify for stored credentials
      scopes.rs         -- Scope enum + extraction from JWT claims
    cache/
      mod.rs            -- TokenRiskReport cache (moka, TTL + broadcast invalidation)
    error.rs            -- ApiError → RFC 7807 IntoResponse impl
    config.rs           -- GatewayConfig, sub-configs (auth, ratelimit, cache, ws)
    ratelimit.rs        -- Per-subject token-bucket rate limiter (in-memory)
    state.rs            -- AppState: Arc-wrapped shared handles
    metrics_registry.rs -- Prometheus registry + counter/histogram definitions
```

`crates/gateway` depends on:
- `crates/common` (AnomalyEvent, Severity, Confidence, TokenMeta)
- `crates/storage` (PgStore — anomaly_events queries for REST feed)
- `crates/detectors` (Detector trait + all D01–D06 implementors)
- `crates/scoring` (ScoringEngine::score())
- `crates/token-registry` (TokenRegistry::enrich())

`crates/gateway` does NOT depend on:
- `crates/chain-adapter` — the gateway is not an indexer; it reads already-ingested data
- `crates/indexer` — same reason; indexer writes to Postgres, gateway reads

Dependency direction: gateway → scoring → detectors → common. Gateway → storage. Gateway
never goes below common, and common never imports gateway.

---

## 3. Endpoint Catalog

### 3.1 REST Endpoints

#### `POST /v1/tokens/analyze`

**Purpose:** On-demand full detector run + scoring for a single (chain, mint). Primary path for
bot-trader pre-trade checks.

**Required scope:** `write:analyze`

**Request body:**
```json
{
  "chain": "solana",
  "mint": "FeqiF7TEVmpYuuj4goD8WgmgyhFgFnZiWtw226wF4hNm",
  "window_hours": 24
}
```

`window_hours` is optional; default 24, max 168 (7 days). The `window` passed to detectors
is `[now - window_hours, now]` where `now` is taken from the request handler (not inside
scoring/detectors — preserving the scoring determinism contract: wall-clock only touches
`TokenRiskReport.computed_at`).

**Response 200:**
```json
{
  "report": { /* TokenRiskReport */ },
  "analysis_duration_ms": 312
}
```

**Response 202 (analysis enqueued — only if async mode is enabled in config):**
Not recommended for MVP. Default: synchronous execution.

**Response 409:**
If an analyze for the same `(chain, mint)` is already in flight, return 409 Conflict with
problem-detail body. Implemented via a per-`(chain, mint)` in-flight set (`DashMap` or
`Mutex<HashSet>`).

**Performance contract:** p99 < 500ms for bot-trader. This drives:
- Detector calls are concurrent (join all D01..D06 with `tokio::join!` or `FuturesUnordered`).
- Postgres queries per detector are bounded by detector design (each fires 1–3 queries).
- scoring `ScoringEngine::score()` is synchronous pure function: < 1ms.
- Cache check before detector run: if a fresh `TokenRiskReport` is in cache (age < cache TTL),
  return it immediately without running detectors. Cache age is included in the response
  envelope as `cache_age_seconds`.

---

#### `GET /v1/tokens/{chain}/{mint}/risk`

**Purpose:** Return a cached or freshly-computed `TokenRiskReport` for a token. Faster than
`POST /v1/tokens/analyze` when the cache is warm.

**Required scope:** `read:risk`

**Path parameters:**
- `chain` — lowercase chain identifier (`"solana"`, `"ethereum"`, `"bsc"`, `"base"`)
- `mint` — chain-canonical token address (Base58 for Solana, checksummed hex for EVM)

**Response 200:**
```json
{
  "report": { /* TokenRiskReport */ },
  "cache_age_seconds": 14,
  "cached": true
}
```

**Response 404:**
If no `TokenRiskReport` is in cache and the token does not exist in Postgres (`tokens` table),
return 404 with `"type": "https://mg-onchain/errors/token-not-found"`.

**Cache miss + no entry:** The gateway does NOT automatically trigger an analyze on a cache
miss for this endpoint. The bot-trader uses `POST /v1/tokens/analyze` for on-demand evaluation;
`GET .../risk` is for consumers who want fast reads against pre-populated cache.

**Rationale:** Mixing the "fast cached read" and "trigger async work" semantics on one endpoint
creates confusing latency behavior for consumers. Custody and exchange, which use this endpoint,
expect bounded latency; they schedule their own analyze calls via `POST` when they need a fresh
report.

---

#### `GET /v1/anomaly_events`

**Purpose:** Paginated historical `AnomalyEvent` feed. Primary path for exchange compliance
and forensic reporting.

**Required scope:** `read:events`

**Query parameters:**

| Parameter | Type | Default | Notes |
|-----------|------|---------|-------|
| `chain` | string | — | Required if `token` specified |
| `token` | string | — | Optional; filter by token address |
| `detector_id` | string | — | Optional; filter by detector |
| `severity_min` | string | `"info"` | Inclusive floor: `"info"`, `"low"`, `"medium"`, `"high"`, `"critical"` |
| `from` | ISO8601 | — | Inclusive; `observed_at >= from` |
| `to` | ISO8601 | now | Exclusive; `observed_at < to` |
| `limit` | integer | 50 | Max 500 |
| `cursor` | string | — | Opaque cursor from previous `next_cursor`; enables stable pagination |

**Cursor design:** The cursor encodes `(observed_at, id)` as a base64-encoded JSON blob.
Postgres query: `WHERE (observed_at, id) < ($cursor_ts, $cursor_id) ORDER BY observed_at DESC, id DESC LIMIT $limit`. This is a keyset cursor that remains stable even while new events are
inserted, unlike OFFSET which degrades under concurrent writes.

**Response 200:**
```json
{
  "events": [ /* AnomalyEvent[] */ ],
  "next_cursor": "eyJvYXQiOiIyMDI2LTA0LTIxVDEyOjAwOjAwWiIsImlkIjo0MjF9",
  "total_in_page": 50
}
```

`next_cursor` is `null` when there are no more results.

**No `total_count`:** A `SELECT COUNT(*)` over the partitioned `anomaly_events` table is
expensive. Exchange consumers iterate via cursor until `next_cursor == null`. A separate
`GET /v1/anomaly_events/count` endpoint can be added in Phase 6 if needed (with appropriate
rate limiting given the cost).

---

#### `GET /v1/detectors`

**Purpose:** Read-only introspection of configured detectors and their thresholds. Consumer
audit, compliance reporting, CI verification.

**Required scope:** `read:events` (lowest privilege; all authenticated consumers can inspect)

**Response 200:**
```json
{
  "detectors": [
    {
      "id": "honeypot_sim",
      "severity_floor": "info",
      "enabled": true,
      "thresholds": {
        "sell_tax_threshold": { "value": 0.5, "rationale": "...", "refs": ["D01/honeypot_sim"] },
        "simulate_paths": { "value": 3, "rationale": "...", "refs": ["D01/honeypot_sim"] }
      },
      "references": ["Torres et al. 2019", "GoPlus fork-state method"]
    }
  ]
}
```

This endpoint reads directly from the in-memory `AllDetectorConfigs` struct (loaded at startup).
No database query. The `references` array is populated from the `refs` field in each
`Threshold<T>`.

---

#### `GET /health`

**Purpose:** Liveness + readiness probe. Used by Docker healthcheck and orchestrator.

**Authentication:** None required (no auth middleware on this path).

**Response 200 (healthy):**
```json
{
  "status": "ok",
  "storage": "ok",
  "scoring": "ok",
  "detectors": "ok",
  "registry": "ok",
  "uptime_seconds": 3712
}
```

**Response 503 (degraded):**
```json
{
  "status": "degraded",
  "storage": "error",
  "storage_detail": "pool timeout after 500ms",
  "scoring": "ok",
  "detectors": "ok",
  "registry": "ok",
  "uptime_seconds": 47
}
```

Health check logic:
- `storage`: execute `SELECT 1` against `PgStore` with 500ms timeout.
- `scoring`: static "ok" (pure function, no external deps).
- `detectors`: "ok" if `AllDetectorConfigs` loaded, "error" if startup failed.
- `registry`: "ok" if `TokenRegistry` holds a live RPC connection, "degraded" if RPC health
  check (last-seen slot within 30s) fails.

The gateway reports readiness only after all startup checks pass. The service must not register
as ready until `storage` and `registry` both succeed.

---

#### `GET /metrics`

**Purpose:** Prometheus text-format metrics scrape endpoint.

**Authentication:** None required for MVP (metrics are not sensitive). Operators may add network
ACL at the load-balancer level. A `metrics_require_auth` config flag enables bearer auth if
needed.

**Response:** `Content-Type: text/plain; version=0.0.4`. Prometheus exposition format.

Core metrics:

| Metric | Type | Labels |
|--------|------|--------|
| `http_requests_total` | Counter | `path`, `method`, `status` |
| `http_request_duration_seconds` | Histogram | `path`, `method` (buckets: 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s) |
| `ws_active_connections` | Gauge | — |
| `ws_subscriptions_active` | Gauge | `chain` |
| `ws_events_dispatched_total` | Counter | `chain`, `detector_id` |
| `ws_lag_notices_total` | Counter | — (when buffer full, dropped events) |
| `scoring_cache_hits_total` | Counter | — |
| `scoring_cache_misses_total` | Counter | — |
| `detector_invocations_total` | Counter | `detector_id`, `outcome` (`ok`, `empty`, `error`, `skipped`) |
| `analyze_in_flight` | Gauge | — (active concurrent analyze operations) |

---

#### `POST /v1/auth/token`

**Purpose:** Exchange username+password credentials for a JWT. Service-to-service consumers
call this once and reuse the token until expiry.

**Authentication:** None (this IS the authentication endpoint).

**Request body:**
```json
{
  "username": "bot-trader-prod",
  "password": "..."
}
```

**Response 200:**
```json
{
  "access_token": "eyJhbGci...",
  "token_type": "Bearer",
  "expires_in": 86400,
  "scopes": ["read:events", "read:risk", "write:analyze"]
}
```

**Response 401:** Invalid credentials.

Credentials are checked against the `auth_users` Postgres table (new migration V00006).
Password is Argon2id-verified against the stored hash. On success, a JWT is minted with the
user's assigned scopes, signed with the Ed25519 private key loaded from config.

---

#### `GET /v1/.well-known/jwks.json`

**Purpose:** Publish the Ed25519 public key for consumer-side JWT verification.

**Authentication:** None.

**Response 200:**
```json
{
  "keys": [
    {
      "kty": "OKP",
      "crv": "Ed25519",
      "x": "<base64url-encoded public key>",
      "kid": "<key id>",
      "use": "sig",
      "alg": "EdDSA"
    }
  ]
}
```

This is the RFC 7517 JSON Web Key Set format. `kid` matches the `kid` claim in issued JWTs.
Consumers can use this to verify JWTs without calling back to the gateway.

---

### 3.2 Admin Endpoints

Admin endpoints are gated by the `admin` scope.

#### `DELETE /v1/admin/cache/{chain}/{mint}`

**Purpose:** Manually invalidate the `TokenRiskReport` cache entry for a specific token.

**Required scope:** `admin`

**Response 200:** `{ "invalidated": true }`
**Response 404:** No cache entry for that token.

---

#### `POST /v1/admin/users`

**Purpose:** Create a new API user. CLI wrapper calls this; not intended for consumer use.

**Required scope:** `admin`

**Request body:**
```json
{
  "username": "market-maker-prod",
  "password": "...",
  "scopes": ["read:events", "read:risk"]
}
```

---

## 4. WebSocket Endpoint

### `GET /v1/ws/stream`

HTTP Upgrade → WebSocket. Serves the real-time `AnomalyEvent` + `TokenRiskReport` push feed.

**Required scope:** `read:events` (bearer token sent in `Authorization` header during the
HTTP Upgrade request, or as `?token=<jwt>` query parameter — axum-tungstenite supports both).

### Connection lifecycle

```
Client                              Gateway
  |                                    |
  |-- HTTP GET /v1/ws/stream --------> |
  |   Authorization: Bearer <jwt>      |
  |                                    |-- validate JWT
  |                                    |-- check scope read:events
  |                                    |-- check ws_connections_per_subject limit
  |<-- 101 Switching Protocols ------- |
  |                                    |
  |-- {"action":"subscribe", ...} ---> |  (subscription message)
  |<-- {"type":"subscribed", ...} ---- |  (ack)
  |                                    |
  |<-- {"type":"event", ...} --------- |  (AnomalyEvent push)
  |<-- {"type":"report", ...} -------- |  (TokenRiskReport push on score delta)
  |                                    |
  |<-- {"type":"ping"} --------------- |  (heartbeat every 30s)
  |-- {"type":"pong"} --------------->  |
  |                                    |
  |-- {"action":"unsubscribe"} ------> |
  |<-- {"type":"unsubscribed"} ------- |
  |                                    |
  |<-- close (4001 lag_overflow) ----- |  (if buffer exhausted)
```

### Subscription message format

Client sends after connect:
```json
{
  "action": "subscribe",
  "chain": "solana",
  "tokens": ["FeqiF7TE...", "WETZjtp..."],
  "detector_ids": ["rug_pull_lp_drain", "pump_dump"],
  "severity_min": "medium",
  "resume_from": "evt_0000000421"
}
```

All fields except `action` are optional:
- Omitting `tokens` subscribes to ALL tokens (exchange use case).
- Omitting `detector_ids` subscribes to all detectors.
- `severity_min` defaults to `"info"`.
- `resume_from` is an opaque event ID from a previous `AnomalyEvent.id` field. On reconnect,
  the gateway replays events from Postgres since that ID within a configurable lookback window
  (default 5 minutes, max 30 minutes). Events older than the lookback are not replayed; a
  `{"type":"replay_truncated","from_id":"evt_...","message":"..."}` notice is sent if truncation
  occurs.

Server ack:
```json
{
  "type": "subscribed",
  "subscription_id": "sub_7f3a9c",
  "effective_filters": {
    "chain": "solana",
    "tokens": ["FeqiF7TE...", "WETZjtp..."],
    "detector_ids": ["rug_pull_lp_drain", "pump_dump"],
    "severity_min": "medium"
  }
}
```

A client may send multiple subscribe messages on the same connection to add subscriptions
(up to `max_subscriptions_per_connection` = 100). Each gets a unique `subscription_id`.

### Push frame formats

**AnomalyEvent push:**
```json
{
  "type": "event",
  "subscription_id": "sub_7f3a9c",
  "event": { /* AnomalyEvent */ }
}
```

**TokenRiskReport push (on score delta):**
```json
{
  "type": "report",
  "subscription_id": "sub_7f3a9c",
  "report": { /* TokenRiskReport */ },
  "previous_score": 0.31,
  "delta": 0.52
}
```

Report pushes are triggered only when `|new_score - prev_score| > ws_report_delta_threshold`
(config default 0.10). This prevents noisy churn when many events land simultaneously for an
already-scored token.

**Heartbeat:**
```json
{ "type": "ping" }
```

Client must respond:
```json
{ "type": "pong" }
```

If no PONG within `heartbeat_timeout_seconds` (60s), the server closes the connection with code
4000.

**Lag notice (backpressure event):**
```json
{
  "type": "lag_notice",
  "dropped": 23,
  "buffer_capacity": 1000,
  "recommendation": "Reduce subscription scope or increase processing speed"
}
```

Sent when the per-subscriber send buffer fills and events are dropped. The gateway drops the
OLDEST events (not newest) — the subscriber gets the most recent state after reconnect.
After sending `lag_notice`, the connection is NOT closed; the subscriber can choose to
reconnect with `resume_from`.

### Backpressure architecture

The internal event bus is a `tokio::sync::broadcast` channel. The gateway's indexer-facing
component receives new `AnomalyEvent`s from Postgres (polled or pushed via `LISTEN/NOTIFY`)
and sends them into the broadcast channel.

Each WS subscriber has a per-connection `mpsc::channel` with bounded capacity
(`send_buffer_capacity` = 1000 messages). The dispatcher task forwards from the broadcast
channel to each subscriber's mpsc channel. If the mpsc send fails (full), the dispatcher
drops the oldest message in the subscriber's buffer (implemented via a ring-buffer wrapper
over the mpsc), increments a drop counter, and sends a `lag_notice` when the count crosses
a threshold (config default: `lag_notice_threshold = 10`).

This architecture means:
- A slow subscriber NEVER blocks the broadcast channel.
- A slow subscriber NEVER blocks other subscribers.
- Backpressure is per-connection, not global.
- The broadcast channel's own capacity (`broadcast_channel_capacity` = 10,000) is the
  gateway-wide buffer; if the indexer-facing ingestion outpaces ALL subscribers combined,
  the broadcast channel lags and the receiver (`lagged()` error) causes the dispatcher to
  log a warning and reload from Postgres.

### Reconnect and event replay

On reconnect, the client sends `resume_from: <last_seen_event_id>`. The gateway:
1. Queries `anomaly_events` table for events since that ID within the lookback window.
2. Sends them as `{"type":"replay","event":...}` frames in order.
3. Then transitions to live push.

`resume_from` is optional. Without it, the subscriber receives only new events.

---

## 5. Authentication Model

### 5.1 JWT Algorithm: EdDSA (Ed25519)

Algorithm: **EdDSA** (IANA name for Ed25519 ECDSA). Using `ed25519-dalek` crate.

Rationale over RSA:
- 64-byte signatures vs 256+ bytes for RSA-2048 — lower token size in Authorization header.
- Key generation is trivially reproducible (`ed25519-dalek::SigningKey::from_bytes`).
- No padding oracle attack surface (RSA PKCS1 / OAEP).
- Widely supported in JWT libraries (`jsonwebtoken = "9"` supports EdDSA natively).

Key rotation: the operator generates a new keypair and restarts the service. Old tokens expire
(default 24h TTL). No hot rotation in MVP.

### 5.2 JWT Claims

```json
{
  "sub": "bot-trader-prod",
  "iss": "mg-onchain",
  "aud": "mg-onchain-api",
  "iat": 1714234567,
  "exp": 1714320967,
  "jti": "7f3a9c12-...",
  "scopes": ["read:events", "read:risk", "write:analyze"]
}
```

`jti` (JWT ID) is a UUID v4, generated at mint time. Stored in `auth_tokens` table for audit
(not for revocation — stateless at MVP). Future revocation list uses `jti` as the key.

### 5.3 Scopes

| Scope | Allows | Consumers |
|-------|--------|-----------|
| `read:events` | `GET /v1/anomaly_events`, `GET /v1/detectors`, WS subscribe | All |
| `read:risk` | `GET /v1/tokens/{chain}/{mint}/risk` | All |
| `write:analyze` | `POST /v1/tokens/analyze` | bot-trader, exchange |
| `admin` | Admin endpoints, manual cache invalidation | Operator only |

Scopes are additive. A user with `["read:events", "read:risk", "write:analyze"]` can access
all non-admin endpoints. Scope checking is a middleware layer that extracts `claims.scopes`
from the validated JWT and checks against the per-route required scope.

### 5.4 User Store (Postgres Migration V00006)

New migration adds:

```sql
CREATE TABLE auth_users (
    id          BIGSERIAL     PRIMARY KEY,
    username    TEXT          NOT NULL UNIQUE,
    -- Argon2id hash, format: $argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>
    password_hash TEXT        NOT NULL,
    scopes      TEXT[]        NOT NULL DEFAULT '{}',
    enabled     BOOLEAN       NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ   NOT NULL DEFAULT NOW()
);

CREATE TABLE auth_tokens (
    jti         UUID          PRIMARY KEY,
    subject     TEXT          NOT NULL,
    issued_at   TIMESTAMPTZ   NOT NULL,
    expires_at  TIMESTAMPTZ   NOT NULL,
    scopes      TEXT[]        NOT NULL
);
CREATE INDEX auth_tokens_subject_idx ON auth_tokens (subject);
CREATE INDEX auth_tokens_expires_idx ON auth_tokens (expires_at);
```

`auth_tokens` is an audit log, not a revocation list. Cleanup job: delete rows where
`expires_at < NOW() - INTERVAL '7 days'`.

### 5.5 Argon2id Parameters

```toml
[gateway.auth.argon2_params]
memory_kib  = 65536   # 64 MiB
iterations  = 3
parallelism = 4
```

These meet OWASP password storage recommendations (2024) for interactive logins. Since these
are service accounts (not humans typing passwords), a one-time hash cost of ~300ms is
acceptable. The hash is computed during `POST /v1/admin/users` by the operator, not during
authentication (authentication only verifies, which is slightly faster due to short-circuit
on mismatch).

### 5.6 mTLS (optional)

A config flag `mtls_required_scopes = ["admin"]` causes the gateway to require client
certificate verification for the listed scopes. When enabled:
- TLS must be active (`tls_cert_path` set).
- The TLS acceptor is configured with `ClientAuth::Required` for connections carrying
  `admin`-scope tokens.
- CA cert for client verification is loaded from `mtls_ca_cert_path`.

Implementation: `rustls` via `axum-server`. mTLS is off by default; enabling it requires
operator-provided client certificates.

### 5.7 Rate Limiting

Per-subject token-bucket in-memory. No Redis.

```toml
[gateway.ratelimit]
default_rpm        = 60     # all scopes not otherwise specified
write_analyze_rpm  = 10     # POST /v1/tokens/analyze (detector run is expensive)
ws_connections_per_subject = 5
```

Implementation: `governor` crate (token bucket, async-aware, no external state). The
per-subject key is the JWT `sub` claim. Anonymous requests (unauthenticated) to health/metrics
use a shared `"_anon"` key with the same `default_rpm`.

On limit exceeded, respond 429 with `Retry-After: <seconds>` header (exact replenish time
from the token bucket state).

---

## 6. Error Shape

RFC 7807 Problem Details (`application/problem+json`). All error responses use this format.

### 6.1 Wire format

```json
{
  "type": "https://mg-onchain/errors/<slug>",
  "title": "Human-readable title",
  "status": 400,
  "detail": "Specific description of what went wrong",
  "instance": "/v1/tokens/analyze",
  "trace_id": "5e3a1f8c9b2d4a07"
}
```

`trace_id` is the request-ID set by `tower-http`'s `RequestIdLayer` (propagated via
`x-request-id` header). If the caller provides `x-request-id`, it is echoed back;
otherwise the gateway generates a UUID.

### 6.2 Error taxonomy

| HTTP status | Type slug | When |
|------------|-----------|------|
| 400 | `invalid-input` | Malformed address, missing required field, unparseable ISO8601, invalid chain name |
| 401 | `unauthenticated` | Missing, expired, or unparseable JWT |
| 403 | `unauthorized` | Valid JWT but missing required scope |
| 404 | `token-not-found` | Token not in `tokens` table + no cached report |
| 409 | `analyze-in-flight` | Same `(chain, mint)` analyze already running |
| 422 | `semantic-error` | Chain not supported (e.g. `"tron"` in Phase 2), window_hours out of range |
| 429 | `rate-limited` | Token bucket exhausted; `Retry-After` header included |
| 500 | `internal-error` | Unexpected panic or unhandled error; logged with full context |
| 503 | `component-unhealthy` | `GET /health` returned degraded + request needs that component |

### 6.3 `ApiError` type in Rust

`ApiError` is a Rust enum with one variant per status class. It implements `axum::response::IntoResponse` which serialises to the RFC 7807 JSON body + correct HTTP status + `Content-Type: application/problem+json`.

```
enum ApiError {
    InvalidInput { detail: String },
    Unauthenticated { detail: String },
    Unauthorized { required_scope: Scope },
    TokenNotFound { chain: Chain, mint: String },
    AnalyzeInFlight { chain: Chain, mint: String },
    SemanticError { detail: String },
    RateLimited { retry_after_seconds: u64 },
    Internal(anyhow::Error),
    ComponentUnhealthy { component: String },
}
```

Handlers return `Result<Json<T>, ApiError>`. The `ApiError::Internal` variant logs at ERROR
level before converting to the wire response; the wire response never includes the internal
error message (only the trace_id for correlation).

---

## 7. Caching Layer

### 7.1 Structure

The cache is a `moka::future::Cache<(Chain, String), Arc<TokenRiskReport>>`:
- Key: `(Chain, canonical_mint_string)`
- Value: `Arc<TokenRiskReport>` (cheap clone to avoid copy in concurrent reads)
- TTL: `token_risk_ttl_seconds` (default 60s)
- Max entries: `token_risk_max_entries` (default 10,000)
- Eviction: LRU + TTL (moka's default policy)

### 7.2 Invalidation

Two invalidation triggers:

**1. TTL expiry** — handled automatically by moka.

**2. Fresh `AnomalyEvent` for token** — When the indexer-facing component receives a new
`AnomalyEvent` for `(chain, token)`, it sends `(chain, token)` on a `tokio::sync::broadcast`
channel (`invalidation_channel`). The cache module subscribes to this channel and calls
`cache.invalidate(&key).await` for any affected entry.

This means: if the indexer ingests a new rug-pull event for token X, the next REST or WS
client to request risk for X will get a freshly-computed report (not a 60-second-old one).

**3. Manual invalidation** — `DELETE /v1/admin/cache/{chain}/{mint}` calls
`cache.invalidate(&key).await` directly.

### 7.3 Cache miss handling

On a cache miss for `POST /v1/tokens/analyze`: run detectors, compute score, insert into cache,
return result.

On a cache miss for `GET /v1/tokens/{chain}/{mint}/risk`: query `anomaly_events` Postgres table
for recent events (last 24h), call `ScoringEngine::score()`, insert into cache, return result.
This means `GET .../risk` is NOT a pure cache-read — it computes on miss. This is safe because:
- The computation is cheap (scoring is a pure function; Postgres query for recent events is fast).
- The result is immediately cached for subsequent calls.

### 7.4 No Redis

Single-instance MVP. Multi-instance deployment (Phase 6+) would require a distributed
invalidation channel. The migration path: replace the `moka` cache with a Redis read-through
cache + Redis Pub/Sub for invalidation events. The cache module is isolated (`cache/mod.rs`)
so the migration is confined to one file.

---

## 8. Observability

### 8.1 Structured Tracing

Use `tracing` + `tracing-subscriber`. Each request has a span with fields:

```rust
#[instrument(
    skip(state),
    fields(
        trace_id = %request_id,
        user_id  = %claims.sub,
        endpoint = %path,
        chain    = %req.chain,
        mint     = %req.mint,
    )
)]
async fn analyze_handler(...) { ... }
```

OTLP export: if `telemetry.otlp_endpoint` is set in config, traces are exported via
`opentelemetry-otlp`. Otherwise, stdout JSON via `tracing-subscriber::fmt::json()`.

Self-hosted compatible: Jaeger / Tempo accept OTLP. No paid SaaS.

### 8.2 Metrics

Prometheus text format at `/metrics`. Implementation: `prometheus` crate (not `axum-prometheus`
which adds a transitive proc-macro dependency on `metrics` crate that conflicts with workspace).

Registry is a `prometheus::Registry` stored in `AppState`. Each handler increments/observes the
relevant metrics defined in `metrics_registry.rs`.

See §3.1 `GET /metrics` for the full metric table.

### 8.3 Log hygiene — secret fields

The following types MUST NOT appear in logs or tracing spans:
- `GatewayConfig.auth.jwt_signing_key_path` contents (the key bytes)
- Argon2id password hashes from `auth_users`
- Raw passwords from `POST /v1/auth/token` request body

Implementation:
- `GatewayConfig` implements `Debug` manually (or via `#[debug(skip)]` field-level macro)
  with signing key bytes replaced by `"<redacted>"`.
- Password in `AuthRequest` is a `secrecy::Secret<String>` — `Debug` and `Display` emit
  `"[REDACTED]"` automatically.
- Never log request bodies at INFO or below for `/v1/auth/token`.

---

## 9. Operational Concerns

### 9.1 Graceful Shutdown

On `SIGTERM` (or `SIGINT` in dev):

1. Stop accepting new connections (axum's `axum::serve(...).with_graceful_shutdown(signal)` API).
2. Wait for in-flight HTTP requests to complete — up to `shutdown_timeout_seconds` (config
   default 30s). After timeout, forcefully close.
3. For WS connections: send a `{"type":"closing","reason":"server_shutdown"}` frame and close
   with code 1001 (Going Away). This gives clients time to reconnect elsewhere.
4. Flush Prometheus metrics (final scrape window).
5. Flush OTLP trace buffer (if configured).
6. Drop `PgStore` pool cleanly (sqlx handles pool shutdown).

The scoring cache is in-process and ephemeral; no flush needed.

### 9.2 Readiness: Startup Ordering

The gateway binary (`crates/server`) starts components in order:

1. Load `config/gateway.toml` → `GatewayConfig`.
2. Load `config/detectors.toml` → `AllDetectorConfigs`.
3. Load `config/scoring.toml` → `ScoringConfig`.
4. Connect `PgStore` — retry up to `db_connect_retries` (default 5) with exponential backoff.
5. Run migrations (`sqlx::migrate!`) if `migrations_auto_apply = true`.
6. Construct `TokenRegistry` with `SolanaRpc`.
7. Initialize Prometheus registry + metrics.
8. Bind TLS / TCP listener.
9. Register readiness (report `GET /health` as `"status": "ok"` only after steps 1–8 all pass).

If any step fails, the binary exits non-zero and the orchestrator restarts it.

### 9.3 SIGHUP Config Reload

On `SIGHUP`, reload non-secret runtime parameters without restart:

- `gateway.ratelimit` (rate limits take effect on next token-bucket replenish)
- `gateway.cache.token_risk_ttl_seconds` (new TTL applied to new cache insertions; existing
  entries expire at old TTL)
- `gateway.ws.ws_report_delta_threshold`
- `gateway.ws.send_buffer_capacity` (applied to new connections; existing connections keep
  old buffer size)

Parameters that require restart (because they affect secret material or listener config):
- `gateway.auth.*` (JWT signing key, Argon2 params)
- `gateway.tls_cert_path`, `gateway.tls_key_path`
- `gateway.bind_address`

Implementation: a `tokio::signal::unix::signal(SignalKind::hangup())` listener in `crates/server`
reloads the config file and updates the relevant fields in `AppState` via `Arc<RwLock<...>>`.
Secret-relevant fields are logged at WARN: "SIGHUP received; skipping reload of [auth] section
— requires restart."

### 9.4 Zero-Downtime Restart

For a zero-downtime restart (deploy new binary):

1. Start new instance on the same port (use `SO_REUSEPORT` + systemd socket activation).
2. New instance reaches readiness.
3. Load balancer (or HAProxy / nginx) detects new instance healthy, routes traffic there.
4. Old instance receives SIGTERM → graceful shutdown (§9.1).

`crates/gateway` does not implement socket hand-off directly — that is an orchestration concern.
The readiness probe (`GET /health`) is the signal the orchestrator uses to switch traffic.

---

## 10. Configuration Shape

`config/gateway.toml` full example:

```toml
# config/gateway.toml
# Gateway configuration for crates/gateway (HTTP + WS API).
# ADR refs: ADR 0003 (self-sovereign), ADR 0001 §D8 (three delivery modes)

[gateway]
bind_address = "0.0.0.0:8080"

# Optional TLS. If unset, HTTP only (suitable for loopback / internal network).
# tls_cert_path = "/etc/mg-onchain/tls/cert.pem"
# tls_key_path  = "/etc/mg-onchain/tls/key.pem"

# Seconds to wait for in-flight requests to complete on SIGTERM.
shutdown_timeout_seconds = 30

# Retry attempts for initial DB connection at startup.
db_connect_retries = 5

[gateway.auth]
# Ed25519 private key (PEM). Generate: openssl genpkey -algorithm ed25519 -out priv.ed25519
jwt_signing_key_path = "/etc/mg-onchain/jwt/priv.ed25519"
jwt_issuer           = "mg-onchain"
jwt_audience         = "mg-onchain-api"
jwt_expiry_hours     = 24

# List of scopes for which client-certificate verification is required.
# Requires TLS to be active. Empty list = mTLS disabled.
mtls_required_scopes = []
# mtls_ca_cert_path  = "/etc/mg-onchain/tls/client-ca.pem"   # needed if mtls_required_scopes is non-empty

[gateway.auth.argon2_params]
memory_kib  = 65536   # 64 MiB — OWASP 2024 recommendation
iterations  = 3
parallelism = 4

[gateway.ratelimit]
# Requests per minute per authenticated subject.
default_rpm = 60
# POST /v1/tokens/analyze is more expensive (runs detectors).
write_analyze_rpm = 10
# Max concurrent WS connections from one subject.
ws_connections_per_subject = 5

[gateway.cache]
token_risk_ttl_seconds = 60
token_risk_max_entries = 10_000

[gateway.ws]
# Server sends ping every N seconds.
heartbeat_interval_seconds = 30
# Client must pong within N seconds or be disconnected.
heartbeat_timeout_seconds  = 60
# Max subscription filters per connection.
max_subscriptions_per_connection = 100
# Per-subscriber send buffer size (messages). Oldest dropped when full.
send_buffer_capacity = 1000
# Only push TokenRiskReport update when score changes by this fraction.
ws_report_delta_threshold = 0.10
# Max event replay window on reconnect (minutes).
replay_lookback_minutes = 5
# Send lag_notice after this many dropped events.
lag_notice_threshold = 10
# Capacity of the internal broadcast channel (gateway-wide).
broadcast_channel_capacity = 10_000

[gateway.telemetry]
# OTLP gRPC endpoint for traces. If unset, stdout JSON.
# otlp_endpoint = "http://localhost:4317"
log_level = "info"

[gateway.metrics]
# If true, require bearer auth to scrape /metrics.
require_auth = false
```

---

## 11. Consumer Interaction Patterns

### 11.1 bot-trader — Pre-trade check

The bot calls `POST /v1/tokens/analyze` before opening a position. Target latency: p99 < 500ms.

Sequence:
1. Bot checks cache: if a fresh report exists for the token (age < 10s), use it directly via
   `GET /v1/tokens/{chain}/{mint}/risk`.
2. Otherwise: `POST /v1/tokens/analyze` → waits for `200 OK` with `TokenRiskReport`.
3. Bot applies its own threshold (e.g. `overall_score < 0.40` AND `overall_severity != "critical"`
   to proceed with trade).

The bot is expected to maintain its own short-lived local cache (in-process) to avoid hammering
the gateway on the same token. The gateway's `cache_age_seconds` field in the response tells
the bot how fresh the cached report is.

### 11.2 mg-custody — Cached risk reads + webhook (Phase 6)

Custody calls `GET /v1/tokens/{chain}/{mint}/risk` for token screening on deposits.

The endpoint is designed for bounded latency: on cache hit, < 5ms. On cache miss, the Postgres
query + scoring takes < 100ms for a token with few events.

Webhook delivery (Phase 6): custody registers a webhook URL via `POST /v1/webhooks/register`.
The gateway pushes Critical `AnomalyEvent`s to the registered URL via HTTP POST. This design
defers webhook delivery because:
- It requires a durable delivery queue (retry on failure).
- mg-custody is not yet integrated.
- The REST polling model is sufficient for Phase 2 use.

### 11.3 Market maker — WS subscription

The MM subscribes to `GET /v1/ws/stream` with `tokens: [list of MM'd tokens]` and
`severity_min: "medium"`.

Reconnect strategy: the MM SDK maintains a reconnect loop with exponential backoff (100ms →
30s). On reconnect, it sends `resume_from: <last_event_id>` to avoid missing events during
the disconnect window.

### 11.4 Exchange — Batch event query + on-demand analyze

**Listing-time check:** Exchange calls `POST /v1/tokens/analyze` when listing a new token.
Response includes full `TokenRiskReport` + detector evidence for compliance documentation.

**Ongoing compliance reporting:** Exchange calls `GET /v1/anomaly_events?severity_min=high&from=<T>&to=<T+1d>&limit=500` + cursor iteration to extract all events for a time range.
Response format is designed for bulk extraction: cursor-based pagination, no count query,
each page is self-contained.

### 11.5 Hardest consumer to satisfy: bot-trader latency vs. exchange bulk throughput

The bot-trader requires p99 < 500ms on `POST /v1/tokens/analyze` (blocking, synchronous,
waiting for six detectors to run). The exchange requires high-throughput cursor pagination
over a large `anomaly_events` table with time-range filters.

These are competing workloads on the same Postgres instance. Mitigations:
- Detector invocations for `POST /v1/tokens/analyze` are concurrent (`FuturesUnordered`) and
  each fires a bounded number of queries. The Postgres connection pool (`max_connections`)
  is shared; bot-trader analyze calls and exchange pagination calls compete for connections.
- Pool sizing: `max_connections = 20` (config) is sufficient for MVP. If contention appears,
  use separate pools for the analyze path vs. the read-only event-feed path (axum extension
  state can carry two `PgPool` handles).
- The `GET /v1/anomaly_events` pagination uses keyset cursor (not OFFSET) so it does not
  degrade with large offsets. Each page is a single bounded-cost query.

No compromise to the API design is required — the latency difference between the two workloads
is managed at the Postgres pool level, not the API level.

---

## 12. Dependencies

Recommended `Cargo.toml` additions for `crates/gateway`:

```toml
[dependencies]
# HTTP server
axum            = { version = "0.8", features = ["macros", "ws"] }
tokio           = { workspace = true }
tower           = "0.5"
tower-http      = { version = "0.6", features = ["trace", "cors", "request-id", "compression-full"] }

# WebSocket
tokio-tungstenite = "0.24"

# Auth
jsonwebtoken    = "9"
argon2          = "0.5"
ed25519-dalek   = { version = "2", features = ["pkcs8", "pem"] }
secrecy         = "0.8"

# Rate limiting
governor        = "0.6"

# Cache
moka            = { version = "0.12", features = ["future"] }

# Metrics
prometheus      = "0.13"

# OpenAPI (code-gen annotations)
utoipa          = { version = "5", features = ["axum_extras"] }
utoipa-swagger-ui = { version = "7", features = ["axum"] }  # optional, dev only

# Serialization (workspace)
serde           = { workspace = true }
serde_json      = { workspace = true }

# TLS (optional, for HTTPS mode)
axum-server     = { version = "0.7", features = ["tls-rustls"] }
rustls          = "0.23"

# Tracing
tracing              = { workspace = true }
tracing-subscriber   = { version = "0.3", features = ["json", "env-filter"] }
opentelemetry        = { version = "0.23", optional = true }
opentelemetry-otlp   = { version = "0.16", optional = true }

# Internal crates
mg-onchain-common         = { path = "../common" }
mg-onchain-storage        = { path = "../storage" }
mg-onchain-detectors      = { path = "../detectors" }
mg-onchain-scoring        = { path = "../scoring" }
mg-onchain-token-registry = { path = "../token-registry" }

[dev-dependencies]
axum-test       = "0.6"    # integration test helpers
tokio           = { workspace = true, features = ["macros", "test-util"] }
```

`utoipa` is the recommended approach for OpenAPI generation from Rust types (derive macros on
handler functions and types). The OpenAPI YAML spec in `docs/api/openapi.yaml` is the
hand-curated version that matches the utoipa output; the authoritative machine-readable spec
is what utoipa generates at build time via `utoipa::OpenApi`.

---

## 13. Developer Acceptance Checklist

The developer task for P5-2 is complete when all of the following pass:

- [ ] `cargo check -p mg-onchain-gateway` passes with no errors.
- [ ] `cargo clippy -p mg-onchain-gateway --all-targets -- -D warnings` passes clean.
- [ ] `cargo test -p mg-onchain-gateway` passes (unit + integration tests, no external services
  except Postgres via `testcontainers`).
- [ ] `POST /v1/tokens/analyze` returns `200` with a `TokenRiskReport` for a token in the
  fixture corpus.
- [ ] `POST /v1/tokens/analyze` returns `409` when called twice concurrently for the same token.
- [ ] `GET /v1/tokens/{chain}/{mint}/risk` returns `404` for an unknown token.
- [ ] `GET /v1/anomaly_events` returns paginated results with correct cursor semantics
  (two-page fetch returns disjoint, ordered, non-overlapping sets).
- [ ] WS connection: subscribe, receive a pushed event, respond to ping with pong, verify
  connection stays alive through two heartbeat cycles.
- [ ] WS backpressure: when subscriber send buffer is full, `lag_notice` is sent instead
  of blocking the dispatch loop.
- [ ] WS reconnect with `resume_from`: events since the given ID are replayed.
- [ ] `GET /health` returns `200` when Postgres is reachable, `503` when it is not.
- [ ] `GET /metrics` returns valid Prometheus text format (`promtool check metrics` passes).
- [ ] JWT validation: expired token returns `401`; missing scope returns `403`.
- [ ] Rate limit: 11th request within a minute for `write_analyze` returns `429` with
  `Retry-After` header.
- [ ] SIGHUP does not crash the service; ratelimit config update takes effect.
- [ ] JWT signing key bytes do not appear in any log output (test via `RUST_LOG=trace`).
- [ ] `docs/api/openapi.yaml` passes `openapi-spec-validator docs/api/openapi.yaml`.
- [ ] The utoipa-generated OpenAPI matches `docs/api/openapi.yaml` (run the validation
  command in CI: `cargo run --bin generate-openapi | diff - docs/api/openapi.yaml`).
- [ ] Migration V00006 (`auth_users`, `auth_tokens` tables) applies cleanly via
  `sqlx migrate run`.

### Testing strategy

- **Unit tests** (`#[cfg(test)]` modules): JWT sign/verify, Argon2 hash/verify,
  cache invalidation logic, rate-limiter token-bucket math, cursor encode/decode.
- **Integration tests** (`tests/` directory, `testcontainers-rs` Postgres): full handler
  tests using `axum-test` client. Each test spins up a Postgres container, runs migrations,
  seeds fixture data, exercises the handler, asserts response shape.
- **No live Solana RPC** in tests: `MockSolanaRpc` from `crates/token-registry` satisfies
  the `SolanaRpc` trait for all gateway tests.
- **OpenAPI validation**: `pip install openapi-spec-validator && openapi-spec-validator docs/api/openapi.yaml` in CI.

---

## 14. Open Questions

**OQ1 — Async analyze with job ID vs. synchronous blocking?**

The current design always runs `POST /v1/tokens/analyze` synchronously (blocks until all
detectors complete). The p99 < 500ms target is achievable for Solana-only Phase 2 (6 detectors,
each 1–3 Postgres queries, concurrent execution). If EVM chains are added in Phase 4 with
simulation RPC calls (D01 honeypot), latency may exceed 500ms. A `202 Accepted + job_id`
async pattern would decouple request latency from detector execution latency but adds
complexity (job state in Postgres, polling endpoint or callback). Decision: keep synchronous
for Phase 2; revisit at Phase 4 when EVM simulation latency is measured.

**OQ2 — WS event dispatch: Postgres LISTEN/NOTIFY vs. polling interval?**

The WS dispatcher needs to know when new `AnomalyEvent` rows are inserted. Two options:
- **LISTEN/NOTIFY**: Postgres `pg_notify('anomaly_events', payload)` called by a trigger or
  by the indexer after each insert. The gateway listens on a separate connection. Sub-second
  delivery latency.
- **Polling**: The gateway polls `SELECT * FROM anomaly_events WHERE id > $last_id` every
  `ws_poll_interval_ms` (e.g. 500ms).

LISTEN/NOTIFY is strictly better for latency but adds a persistent Postgres connection and
a trigger (or application-level notify call from the indexer). Polling is simpler to reason
about and survives Postgres reconnects without special handling. For Phase 2 (dozens of
tracked tokens), polling at 500ms is acceptable. LISTEN/NOTIFY is the Phase 3 upgrade path.
Decision: **polling at 500ms** for P5-2; document LISTEN/NOTIFY as a Phase 3 upgrade in
`ROADMAP.md`.

**OQ3 — Scope granularity: is `write:analyze` the right name?**

`write:analyze` suggests a write operation, but `POST /v1/tokens/analyze` does not write
durable state (it reads Postgres, runs detectors in memory, and caches the result). A better
name might be `execute:analyze`. However, `write:` prefix is a well-established OAuth2
convention (GitHub uses `read:`/`write:` scopes). Changing to `execute:` deviates from
convention. Proposed: keep `write:analyze` as the scope name but document in the OpenAPI spec
that "write" in this context means "trigger expensive computation" not "mutate persistent state."

**OQ4 — Cache warming on startup?**

On cold start, the cache is empty. The first request for any token triggers a Postgres query +
scoring computation. For the exchange use case (screening hundreds of tokens at listing time),
this means a burst of cold-cache requests. Option A: pre-warm cache by scanning the `tokens`
table at startup and scoring each tracked token. Option B: accept cold-start latency; the
first request per token pays the miss penalty once. Option A adds startup complexity and
delays readiness. Option B is simpler. Decision: accept cold-start misses for Phase 2; add
cache pre-warming as a `--warm-cache` startup flag in Phase 4.

**OQ5 — `GET /v1/anomaly_events` without `chain` filter: cross-chain query?**

The endpoint allows omitting `chain` to retrieve events across all chains. In Phase 2 (Solana
only), this is trivially correct. In Phase 4+ (multiple chains), a cross-chain query hits all
partition ranges and may be expensive. Option: require `chain` in Phase 4; treat omission as
`chain=all` with a stricter rate limit and an explicit `max_limit = 100` for cross-chain
requests. Document this in the OpenAPI spec as a planned breaking change.

---

## 15. Design Gaps

**DG1 — Webhook delivery for mg-custody is deferred**

`mg-custody` needs push notification on Critical events (deposit screening). The current
design provides only REST polling. Phase 6 adds `POST /v1/webhooks/register` + a durable
delivery queue (Postgres-backed retry). The gap: custody may miss Critical events between
polling intervals. Mitigation: custody polls `GET /v1/anomaly_events?severity_min=critical`
every 30s; at most 30s event lag, acceptable for custody's use case.

**DG2 — No bulk analyze endpoint**

The exchange needs to screen all tokens in its listing queue at startup (potentially hundreds).
`POST /v1/tokens/analyze` is a single-token endpoint. A `POST /v1/tokens/analyze/batch` that
accepts an array of `{chain, mint}` inputs and returns an array of `TokenRiskReport`s would
serve this use case without N separate HTTP calls. Deferred: Phase 6. Mitigation for Phase 2:
exchange calls `POST /v1/tokens/analyze` concurrently with a 10-RPS self-imposed rate limit.

**DG3 — WS `resume_from` relies on event IDs being stable**

`resume_from` uses the `AnomalyEvent` row `id` (Postgres `BIGSERIAL`). If the Postgres
instance is replaced (failover, dump/restore), `id` sequences reset or differ. A more robust
replay cursor would use `(observed_at, chain, token, detector_id)` as a composite key —
immune to sequence resets. Phase 6 concern; BIGSERIAL IDs are fine for single-instance MVP.

**DG4 — moka cache is lost on restart**

On graceful restart, the in-process `moka` cache is dropped. All consumers see cache misses
until the cache re-warms. For bot-trader (p99 < 500ms), the first-request-after-restart latency
may briefly exceed the target. Mitigation: the `GET /health` readiness endpoint is unhealthy
until the service is ready; the load balancer keeps routing to the old instance until the new
one is warm. See §9.2.

**DG5 — `detectors.rs` threshold endpoint exposes full threshold rationale to all `read:events` consumers**

The `GET /v1/detectors` endpoint returns detector thresholds including `rationale` strings and
REFERENCES.md entry IDs. This is intentional for transparency (ADR 0001 §D6 "trust moat").
However, it also exposes internal scoring weights and detection methodology to anyone with
a `read:events` token. A determined adversary could calibrate evasion strategies against the
published thresholds. Mitigation for MVP: this is not a secret (the methodology is published
in REFERENCES.md anyway). A `read:admin` scope could restrict threshold detail in Phase 6 if
operational security concerns emerge.

---

*End of design. References: ADR 0001 §D4/D8, ADR 0002, ADR 0003, docs/designs/0003-detector-trait.md, docs/designs/0010-scoring.md, crates/common/src/anomaly.rs, crates/common/src/token.rs, crates/scoring/src/lib.rs, crates/storage/src/pg.rs.*

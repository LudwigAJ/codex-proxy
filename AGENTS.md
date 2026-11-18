# codex-proxy — Agent Guide

## Purpose

`codex-proxy` is a Rust daemon and CLI that authenticates a single user with
ChatGPT OAuth, generates a local fake API key, and exposes an OpenAI-compatible
HTTP endpoint. Coding tools point at this proxy instead of the real OpenAI
platform and get requests transparently forwarded to the ChatGPT Codex backend.

The intended product shape:

1. User runs `codex-proxy auth` once — OAuth PKCE flow, tokens persisted.
2. User runs `codex-proxy start` — proxy daemon listens on `127.0.0.1:8317`.
3. Coding tool configured with `base_url=http://127.0.0.1:8317` and the proxy
   API key printed by `codex-proxy auth`.

---

## Repository Layout

```
src/
  main.rs                  — binary entry point
  lib.rs                   — crate root, module exports
  app.rs                   — run(), CLI dispatch, tracing init
  cli.rs                   — Clap CLI definitions (Cli, StartArgs, AuthArgs)
  config.rs                — Config struct and all sub-sections (TOML)
  state.rs                 — ProxyState, TokenStore, fake-key generation
  storage.rs               — read_json / write_json / write_string helpers
  paths.rs                 — AppPaths: centralised file path management
  constants.rs             — CALLBACK_PATH, FAKE_API_KEY_PREFIX, TOKEN_REFRESH_WINDOW_SECONDS
  time.rs                  — unix_timestamp_secs()
  browser.rs               — open_browser_url() wrapper
  commands/
    mod.rs
    start.rs               — `codex-proxy start` handler
    auth.rs                — `codex-proxy auth` handler (full OAuth flow)
  auth/
    mod.rs                 — PKCE generation, token exchange, refresh, JWT decode
    callback.rs            — temporary Axum server for OAuth callback
  proxy/
    mod.rs                 — Axum router, ProxyAppState, route handlers, auth gate
    forwarding.rs          — request normalization, upstream forwarding, SSE handling
    websocket.rs           — GET /v1/responses WebSocket compatibility transport
    chat_completions.rs    — POST /v1/chat/completions compatibility adapter

tests/
  fixtures/
    responses_request_input.json    — example un-normalized /v1/responses body
    responses_request_expected.json — expected body after normalization

Cargo.toml
config.example.toml        — reference config mirroring compiled-in defaults
config.docker.toml         — Docker-specific runtime config baked into the image
Dockerfile                 — multi-stage build (builder + minimal runtime)
docker-compose.yml         — compose example with named volume for /app/data
scripts/
  release-check.sh         — automated release validation helper
```

---

## Runtime Contract

These files are written and read across daemon restarts. **Do not change their
paths or schema without a deliberate migration.**

| File | Contents |
|------|----------|
| `./config.toml` | Runtime configuration (TOML). Auto-created on first start. |
| `./data/state.json` | Fake API key + pending OAuth flow state. |
| `./data/tokens.json` | OAuth access token, refresh token, expiry. |

Inside Docker (`WORKDIR /app`) the same relative paths resolve to
`/app/config.toml`, `/app/data/state.json`, `/app/data/tokens.json`.

---

## Key Concepts

### Fake API Key
Generated once via `openssl rand -hex 24`, prefixed `sk-codex-proxy-`, stored
in `state.json`. Clients send this key as `Authorization: Bearer <key>` or
`x-api-key: <key>`. The proxy validates it before forwarding any request.

### Token Lifecycle
Tokens are classified as `Missing | Valid | Expiring | Expired` using
`TOKEN_REFRESH_WINDOW_SECONDS = 300`. On daemon startup and on every forwarding
request the proxy checks this status and attempts a refresh when the token is
expiring or expired. Refreshed tokens are persisted to `tokens.json` and
updated in the in-memory `Arc<RwLock<Option<TokenStore>>>`.

### Request Normalization (`prepare_request_body`)
Every responses request forwarded upstream is normalized before forwarding:
- Model name canonicalized via `ModelsConfig::canonicalize` (alias map + fallback).
- `store: false` forced (backend must not cache).
- `stream: true` forced (proxy always streams from backend).
- `item_reference` items removed from the `input` array.
- `id` fields stripped from all remaining `input` items.
- Orphaned `function_call_output` items (whose pair was removed) dropped.
- `reasoning.encrypted_content` added to the `include` array if missing.

### Chat Completions Adapter
`POST /v1/chat/completions` is translated into the responses format by
`translate_chat_completions_request`, forwarded via the same normalization
pipeline, then the response is converted back by `responses_to_chat_completion`.
Streaming is not yet supported on this route.

### WebSocket Transport
`GET /v1/responses` upgrades to a WebSocket for Codex-style clients. Incoming
`response.create` / `response.append` messages are converted back into the same
responses request pipeline used by `POST /v1/responses`, and upstream SSE
`data:` lines are forwarded to the client as WebSocket text frames.

### SSE Handling
The upstream always returns an SSE stream. For streaming HTTP clients the proxy
adds periodic `: keep-alive` comments and injects an `event: error` frame if
the upstream stream fails mid-flight. For non-streaming HTTP clients the proxy
sends leading JSON whitespace keep-alives while waiting, then
`parse_sse_final_response` scans for a `response.done` or
`response.completed` event and returns its `response` field as JSON.

---

## HTTP API

| Method | Path | Auth required | Notes |
|--------|------|---------------|-------|
| GET | `/healthz` | No | Rich JSON status (token_status, forwarding_ready, …) |
| GET | `/readyz` | No | 200 = forwarding ready, 503 = not ready |
| GET | `/v1/models` | Yes | Lists `config.models.advertised` |
| GET | `/v1/models/{model_id}` | Yes | 404 if model not in advertised list |
| GET | `/v1/responses` | Yes | WebSocket compatibility transport |
| POST | `/v1/responses` | Yes | Primary forwarding route |
| POST | `/v1/responses/compact` | Yes | Compact response forwarding route |
| POST | `/v1/chat/completions` | Yes | Non-streaming compat adapter |

---

## Configuration Reference

All sections have sensible defaults and auto-serialize to `config.toml` on
first run. See `config.example.toml` for an annotated reference.

| Key | Default | Description |
|-----|---------|-------------|
| `server.bind` | `127.0.0.1` | Proxy listen host |
| `server.port` | `8317` | Proxy listen port |
| `server.log_filter` | `info` | tracing filter (same as RUST_LOG) |
| HTTP request body limit | `200 MiB` | Fixed in `src/proxy/mod.rs` |
| `auth.callback_bind_host` | `127.0.0.1` | OAuth callback listener bind host for local runs |
| `auth.redirect_host` | `localhost` | Host embedded in the OAuth redirect URI |
| `auth.callback_port` | `1455` | OAuth callback listener port |
| `auth.open_browser` | `true` | Auto-open browser during auth |
| `auth.timeout_seconds` | `300` | Seconds to wait for OAuth callback |
| `upstream.responses_base_url` | `https://chatgpt.com/backend-api` | ChatGPT backend |
| `upstream.codex_responses_path` | `/codex/responses` | Codex endpoint path |
| `upstream.codex_compact_path` | `/codex/responses/compact` | Codex compact endpoint path |
| `upstream.keep_alive_interval_secs` | `15` | HTTP keep-alive heartbeat interval; `0` disables it |
| `models.advertised` | `["gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"]` | Models returned by /v1/models |
| `models.fallback` | `gpt-5.4` | Model used when no alias matches |
| `models.aliases` | see config.example.toml | Provider-prefixed → canonical name map |

---

## Working Rules

- Keep the path contract stable unless a migration is part of the task.
- Update `README.md`, `AGENTS.md`, and `config.example.toml` together when
  changing runtime behaviour or architecture.
- Prefer small, testable diffs over broad rewrites.
- Add or update tests when adding normalization logic or changing the request
  pipeline — the fixture files in `tests/fixtures/` are the canonical input/output
  contract for `prepare_request_body`.

## Validation Baseline

Before closing meaningful changes, run:

```bash
cargo fmt
cargo check
cargo test
```

When Docker or packaging changes are involved, also run:

```bash
docker build -t codex-proxy:latest .
./scripts/release-check.sh
```

For Docker auth changes, verify the image-level config too:

```bash
docker run --rm --entrypoint cat codex-proxy:latest /app/config.toml
```

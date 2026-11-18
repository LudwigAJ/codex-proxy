# codex-proxy

`codex-proxy` is a local Rust daemon that authenticates you with ChatGPT via
OAuth and exposes an OpenAI-compatible API endpoint. Point any coding tool that
speaks the OpenAI API at `http://127.0.0.1:8317` and it will be transparently
proxied to the ChatGPT Codex backend - no real OpenAI API key required.

```
 Coding tool -> (proxy API key) -> codex-proxy -> (OAuth token) -> ChatGPT Codex
```

---

## Table of Contents

- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Quickstart — local binary](#quickstart--local-binary)
- [Quickstart — Docker Compose](#quickstart--docker-compose)
- [Authentication](#authentication)
- [Connecting a coding tool](#connecting-a-coding-tool)
- [CLI reference](#cli-reference)
- [Configuration](#configuration)
- [HTTP API](#http-api)
- [Known limitations](#known-limitations)
- [Release validation](#release-validation)

---

## Prerequisites

**Local binary**
- Rust 1.94+ (`rustup` recommended)
- `openssl` on `PATH` (used to generate the proxy API key)

**Docker**
- Docker Engine 24+ or Docker Desktop

---

## Installation

### Build from source

```bash
git clone <repo-url>
cd codex-proxy
cargo build --release
# Binary is at ./target/release/codex-proxy
```

To run `codex-proxy` from any terminal, symlink the built binary into
`~/.local/bin` and add that directory to your `PATH`:

```bash
mkdir -p "$HOME/.local/bin"
ln -sf "$PWD/target/release/codex-proxy" "$HOME/.local/bin/codex-proxy"
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
export PATH="$HOME/.local/bin:$PATH"
codex-proxy --help
```

If you use `bash`, add the export line to `~/.bashrc` instead of `~/.zshrc`.
If you skip this step, replace `codex-proxy` below with
`./target/release/codex-proxy`.

### Docker image

```bash
docker build -t codex-proxy:latest .
```

---

## Quickstart — local binary

### 1. Authenticate

Run the auth flow once. This opens a browser window where you log in to
ChatGPT and grant access. Tokens are saved to `./data/tokens.json`.

```bash
codex-proxy auth
```

After successful login the command prints your **proxy API key**:

```
Authentication complete.
Use this proxy API key with local clients:

sk-codex-proxy-<hex>
```

Copy and save this key — you will use it as the API key in your coding tool.

### 2. Start the daemon

```bash
codex-proxy start
```

The proxy now listens on `http://127.0.0.1:8317`.

### 3. Verify

```bash
curl http://127.0.0.1:8317/healthz
```

Look for `"forwarding_ready": true` in the response. If `auth_ready` is
`false`, run `auth` again.

---

## Quickstart — Docker Compose

The included `docker-compose.yml` bundles the default config and persists data
in a named Docker volume so tokens survive container restarts.

### 1. Start the container

```bash
docker compose up --build -d
```

The proxy starts on port `8317` of your host machine.

### 2. Authenticate inside the container

Because the OAuth callback listener runs inside the container, you need to
authenticate via `docker exec` and handle the browser step manually:

```bash
docker exec -it codex-proxy \
  codex-proxy auth --no-browser
```

> **Note**: the bundled Compose file sets `container_name: codex-proxy`. If
> your local setup differs, run `docker ps` to confirm the exact name.

If you prefer Compose-aware exec, use:

```bash
docker compose exec codex-proxy \
  codex-proxy auth --no-browser
```

The command prints an authorization URL:

```
Open this URL to authenticate codex-proxy:

https://auth.openai.com/oauth/authorize?...
```

Open that URL in a browser on your host machine. After you log in and grant
access, ChatGPT redirects to `http://localhost:1455/auth/callback`. The
container image now ships with a Docker-specific config that binds the
temporary callback listener to `0.0.0.0`, so the published callback port can
reach it without extra flags.

If the browser reaches that `localhost` callback URL but the page itself still
fails to load, `codex-proxy auth` now supports a paste fallback:

- If the auth command is still waiting in the terminal, paste the full callback
  URL from the browser address bar into that same terminal and press Enter.
- Or, from a second terminal, run:

```bash
docker compose exec codex-proxy \
  codex-proxy auth --callback-url 'http://localhost:1455/auth/callback?...'
```

The callback port must also be published. Add or confirm this in your
`docker-compose.yml`:

```yaml
services:
  codex-proxy:
    ports:
      - "127.0.0.1:8317:8317"
      - "127.0.0.1:1455:1455"   # OAuth callback port
```

Then re-run `docker compose up --build -d` before running `auth`.

After the browser redirect completes the command prints the proxy API key:

```
Authentication complete.
Use this proxy API key with local clients:

sk-codex-proxy-<hex>
```

### 3. Verify

```bash
curl http://127.0.0.1:8317/healthz
```

### 4. Stop

```bash
docker compose down
```

Token data is preserved in the `codex-proxy-data` Docker volume. To reset
everything (including tokens):

```bash
docker compose down -v
```

### Troubleshooting — port not reachable on macOS

If the container shows as healthy but `curl http://127.0.0.1:8317/healthz`
returns "Connection refused", Docker Desktop's userland proxy has silently
failed to bind the port. This is a known Docker Desktop bug on macOS — the
port appears mapped in `docker compose ps` but nothing is actually listening
on the host.

**Fix**: use [Colima](https://github.com/abiosoft/colima) instead of Docker
Desktop:

```bash
brew install colima docker docker-compose
colima start
docker compose up --build -d
```

Colima uses a different networking stack and does not have this issue.

### Overriding the bundled config

The container image ships with a bundled `/app/config.toml` generated from
`config.docker.toml`. That Docker-specific config keeps:

- `server.bind = "0.0.0.0"`
- `auth.callback_bind_host = "0.0.0.0"`
- `auth.redirect_host = "localhost"`

If you mount your own config into the container, keep those Docker auth
settings or the browser callback can fail to reach the temporary listener.

Mount your own file like this:

```yaml
services:
  codex-proxy:
    volumes:
      - ./config.toml:/app/config.toml:ro
      - codex-proxy-data:/app/data

volumes:
  codex-proxy-data:
```

---

## Authentication

`codex-proxy auth` runs the ChatGPT OAuth PKCE flow:

1. Generates a PKCE verifier/challenge pair.
2. Opens `https://auth.openai.com/oauth/authorize?...` in your browser
   (or prints the URL if `--no-browser` is set).
3. Starts a local HTTP server on `callback_bind_host:callback_port`
   (`127.0.0.1:1455` by default) and registers the OAuth redirect URI
   `http://localhost:1455/auth/callback` by default.
4. Accepts either the direct callback or a pasted callback URL fallback when
   the browser cannot load the `localhost` callback page.
5. Exchanges the authorization code for access + refresh tokens.
6. Saves tokens to `data/tokens.json` and prints the proxy API key.

### Token reuse and refresh

`codex-proxy auth` reuses valid stored tokens and silently refreshes expiring
or expired ones before opening a browser. Use `--force` to start a fresh login
regardless:

```bash
codex-proxy auth --force
```

The daemon also attempts a background refresh at startup and during forwarding
when the token is within 5 minutes of expiry.

---

## Connecting a coding tool

Configure your tool with:

| Setting | Value |
|---------|-------|
| Base URL | `http://127.0.0.1:8317/v1` |
| API key | The `sk-codex-proxy-<hex>` key printed by `auth` |

> **Note**: always include the `/v1` suffix in the base URL. Tools that speak
> the OpenAI API append paths like `/chat/completions` or `/responses` directly
> to the base URL, so omitting `/v1` results in 404s.

### Example — OpenAI Python SDK

```python
from openai import OpenAI

client = OpenAI(
    api_key="sk-codex-proxy-<your-key>",
    base_url="http://127.0.0.1:8317/v1",
)

response = client.chat.completions.create(
    model="gpt-5.4",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(response.choices[0].message.content)
```

### Example — curl

```bash
curl http://127.0.0.1:8317/v1/responses \
  -H "Authorization: Bearer sk-codex-proxy-<your-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-5.4",
    "input": [{"type": "message", "role": "user", "content": "Hello!"}]
  }'
```

### Example — Droid

Add a custom model entry to `~/.factory/settings.json`:

```json
{
  "customModels": [
    {
      "model": "gpt-5.4",
      "id": "custom:GPT-5.4",
      "index": 1,
      "baseUrl": "http://127.0.0.1:8317/v1",
      "apiKey": "sk-codex-proxy-<your-key>",
      "displayName": "GPT-5.4",
      "maxOutputTokens": 128000,
      "noImageSupport": false,
      "provider": "openai"
    }
  ]
}
```

Replace `sk-codex-proxy-<your-key>` with the key printed by `codex-proxy auth`. The model will appear as **GPT-5.4** in Droid's model picker.

---

## CLI reference

```
codex-proxy [OPTIONS] <COMMAND>

Options:
  --config-path <PATH>   Path to config.toml [default: ./config.toml]
  --data-dir <DIR>       Directory for state.json and tokens.json [default: ./data]

Commands:
  start   Start the local proxy daemon
  auth    Authenticate with ChatGPT OAuth and persist tokens
```

### `start` flags

```
--bind <HOST>   Override the listen host (default: value in config.toml)
--port <PORT>   Override the listen port (default: value in config.toml)
```

### `auth` flags

```
--no-browser          Print the authorization URL instead of opening a browser
--force               Start a new login even when stored tokens are valid
--callback-url <URL>  Complete auth using a pasted callback URL
--callback-host <HOST>  Override the OAuth redirect host
--callback-bind-host <HOST>  Override the OAuth callback bind host
--callback-port <PORT>  Override the OAuth callback port
```

---

## Configuration

If `config.toml` does not exist, the daemon creates it with defaults on first
start. See [`config.example.toml`](./config.example.toml) for local/runtime
defaults and [`config.docker.toml`](./config.docker.toml) for the container
defaults baked into the Docker image.

HTTP request bodies are limited to 200 MiB. This is a fixed server limit and
is not currently configurable through `config.toml`.

### Key settings

```toml
[server]
bind = "127.0.0.1"   # proxy listen host
port = 8317           # proxy listen port
log_filter = "info"   # tracing filter (same syntax as RUST_LOG)

[auth]
callback_bind_host = "127.0.0.1"
redirect_host = "localhost"
callback_port = 1455
open_browser = true
timeout_seconds = 300

[upstream]
keep_alive_interval_secs = 15

[models]
advertised = ["gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"]
fallback = "gpt-5.4"

[models.aliases]
"openai/gpt-5.4" = "gpt-5.4"
"openai/gpt-5.4-mini" = "gpt-5.4-mini"
"openai/gpt-5.3-codex" = "gpt-5.3-codex"
```

**Model normalization**: client-sent model names are matched against
`advertised` (case-insensitive), then against `aliases`, then fall back to
`fallback`. This means tools sending `openai/gpt-5.4` or `gpt-5.4` both work.

**Log level**: override at runtime without editing config using:

```bash
RUST_LOG=debug codex-proxy start
```

**Docker auth**: the container image uses a Docker-specific `/app/config.toml`
that keeps `redirect_host = "localhost"` and sets
`callback_bind_host = "0.0.0.0"`. That is why the plain command works:

```bash
docker compose exec codex-proxy codex-proxy auth --no-browser
```

If your browser still cannot load the `localhost:1455` callback page in that
setup, paste the final callback URL into the same waiting terminal or run:

```bash
docker compose exec codex-proxy \
  codex-proxy auth --callback-url 'http://localhost:1455/auth/callback?...'
```

---

## HTTP API

All endpoints except `/healthz` and `/readyz` require the proxy API key:

```
Authorization: Bearer sk-codex-proxy-<hex>
# or
x-api-key: sk-codex-proxy-<hex>
```

| Method | Path | Description |
|--------|------|-------------|
| GET | `/healthz` | Full status JSON (auth, forwarding, token status) |
| GET | `/readyz` | `200 OK` when forwarding is ready, `503` otherwise |
| GET | `/v1/models` | OpenAI-compatible model list |
| GET | `/v1/models/{model_id}` | Single model object or 404 |
| GET | `/v1/responses` | WebSocket compatibility transport |
| POST | `/v1/responses` | Primary forwarding route (streaming supported) |
| POST | `/v1/responses/compact` | Compact response forwarding route |
| POST | `/v1/chat/completions` | Non-streaming compat adapter |

### Health check response

```json
{
  "status": "ok",
  "forwarding_ready": true,
  "auth_ready": true,
  "token_status": "valid",
  "fake_api_key_configured": true,
  "chat_completions_compatibility": {
    "implemented": true,
    "streaming_supported": false
  }
}
```

---

## Runtime file layout

```
./config.toml          — TOML configuration (auto-created on first run)
./data/state.json      — proxy API key and pending OAuth state
./data/tokens.json     — OAuth access and refresh tokens
```

Inside Docker (`WORKDIR /app`):

```
/app/config.toml
/app/data/state.json
/app/data/tokens.json
```

---

## Known limitations

- `POST /v1/chat/completions` does not support streaming. Use `POST /v1/responses`
  for streaming.
- `GET /v1/responses` is a WebSocket transport intended for Codex-style clients;
  standard OpenAI SDKs should keep using the HTTP `POST /v1/responses` route.
- The proxy advertises only the models listed in `config.toml`; it does not
  mirror the full OpenAI model catalog.
- Docker auth requires publishing the OAuth callback port (`1455` by default)
  and using `--no-browser` with `docker exec`.

---

## Release validation

```bash
# Unit and integration tests
cargo test

# Automated release checks (build + health check)
./scripts/release-check.sh

# Manual Docker checks
docker build -t codex-proxy:latest .
docker run --rm -p 127.0.0.1:8317:8317 codex-proxy:latest
curl http://127.0.0.1:8317/healthz

docker compose up --build
curl http://127.0.0.1:8317/healthz
docker compose down -v
```

---

## Disclaimer

`codex-proxy` is an independent open-source project and is not affiliated with,
endorsed by, or associated with OpenAI, Codex, or any of their subsidiaries or
related entities. All product names, trademarks, and registered trademarks are
the property of their respective owners.

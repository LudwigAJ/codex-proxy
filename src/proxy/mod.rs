//! Local OpenAI-compatible proxy server.
//!
//! Exposes the following HTTP routes:
//!
//! | Method | Path                      | Description                                          |
//! |--------|---------------------------|------------------------------------------------------|
//! | GET    | `/healthz`                | Returns rich JSON health/status payload              |
//! | GET    | `/readyz`                 | 200 when forwarding is ready, 503 otherwise          |
//! | GET    | `/v1/models`              | Lists advertised models (OpenAI-compatible)          |
//! | GET    | `/v1/models/{model_id}`   | Returns a single model object or 404                 |
//! | GET    | `/v1/responses`           | WebSocket compatibility transport for responses      |
//! | POST   | `/v1/responses`           | Forwards requests to the ChatGPT Codex backend       |
//! | POST   | `/v1/responses/compact`   | Forwards compaction requests to the Codex backend    |
//! | POST   | `/v1/chat/completions`    | Non-streaming chat completions compatibility adapter |
//!
//! All routes except `/healthz` and `/readyz` require the fake API key in
//! either `Authorization: Bearer <key>` or `x-api-key: <key>`.

mod chat_completions;
mod forwarding;
mod websocket;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
    config::Config,
    paths::AppPaths,
    state::{TokenStore, stored_auth_status},
};

use self::chat_completions::forward_chat_completions;
use self::forwarding::{ForwardingState, forward_responses, forward_responses_compact};
use self::websocket::forward_v1_responses_websocket;

/// Maximum size of a buffered HTTP request body (200 MiB).
const MAX_REQUEST_BODY_BYTES: usize = 200 * 1024 * 1024;

/// Shared state cloned into every Axum handler via [`axum::extract::State`].
///
/// Tokens are wrapped in `Arc<RwLock<...>>` so that background refresh
/// (performed inside request handlers) can update them without rebuilding
/// the entire state.
#[derive(Debug, Clone)]
pub struct ProxyAppState {
    /// Full runtime configuration loaded from `config.toml`.
    pub config: Config,
    /// Resolved filesystem paths for config and data files.
    pub paths: AppPaths,
    /// Fake API key that clients must present to use the proxy.
    pub fake_api_key: String,
    /// Shared HTTP client reused across all upstream requests.
    pub client: Client,
    /// Current OAuth tokens, updated in place when a refresh succeeds.
    pub tokens: Arc<RwLock<Option<TokenStore>>>,
}

impl ProxyAppState {
    pub fn new(
        config: Config,
        paths: AppPaths,
        fake_api_key: String,
        tokens: Option<TokenStore>,
    ) -> Self {
        Self {
            config,
            paths,
            fake_api_key,
            client: Client::new(),
            tokens: Arc::new(RwLock::new(tokens)),
        }
    }

    /// Extract the subset of state needed by the forwarding layer.
    fn forwarding_state(&self) -> ForwardingState {
        ForwardingState {
            config: self.config.clone(),
            paths: self.paths.clone(),
            client: self.client.clone(),
            tokens: Arc::clone(&self.tokens),
        }
    }
}

pub async fn serve(bind: String, port: u16, state: ProxyAppState) -> Result<()> {
    let listener = TcpListener::bind(format!("{bind}:{port}"))
        .await
        .with_context(|| format!("failed to bind proxy server on {bind}:{port}"))?;
    let local_addr = listener.local_addr().context("failed to read local addr")?;

    tracing::info!("codex-proxy listening on http://{local_addr}");
    let app = router(state);

    axum::serve(listener, app)
        .await
        .context("proxy server exited unexpectedly")
}

pub fn router(state: ProxyAppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{model_id}", get(get_model))
        .route(
            "/v1/responses",
            get(forward_v1_responses_websocket).post(forward_v1_responses),
        )
        .route("/v1/responses/compact", post(forward_v1_responses_compact))
        .route("/v1/chat/completions", post(forward_v1_chat_completions))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz(State(state): State<ProxyAppState>) -> Json<Value> {
    let tokens = state.tokens.read().await;
    Json(health_payload(
        tokens.as_ref(),
        !state.fake_api_key.is_empty(),
        true,
    ))
}

async fn readyz(State(state): State<ProxyAppState>) -> Response {
    let tokens = state.tokens.read().await;
    let payload = health_payload(tokens.as_ref(), !state.fake_api_key.is_empty(), true);
    let forwarding_ready = payload
        .get("forwarding_ready")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let status = if forwarding_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(payload)).into_response()
}

/// Build the JSON payload returned by `/healthz` and `/readyz`.
///
/// `forwarding_ready` is `true` only when all three preconditions hold:
/// a fake API key is configured, a valid or expiring OAuth token exists, and
/// the responses route is ready. This is used by `readyz` to gate traffic.
fn health_payload(
    tokens: Option<&TokenStore>,
    fake_api_key_configured: bool,
    responses_route_ready: bool,
) -> Value {
    let token_status = stored_auth_status(tokens);
    let auth_ready = matches!(
        token_status,
        crate::state::StoredAuthStatus::Valid | crate::state::StoredAuthStatus::Expiring
    );
    let forwarding_ready = fake_api_key_configured && auth_ready && responses_route_ready;

    json!({
        "status": "ok",
        "service": "codex-proxy",
        "version": env!("CARGO_PKG_VERSION"),
        "skeleton_mode": false,
        "forwarding_implemented": true,
        "daemon_ready": true,
        "auth_ready": auth_ready,
        "forwarding_ready": forwarding_ready,
        "responses_route_ready": responses_route_ready,
        "chat_completions_compatibility": {
            "implemented": true,
            "streaming_supported": false
        },
        "authenticated": auth_ready,
        "token_status": token_status.as_str(),
        "fake_api_key_configured": fake_api_key_configured,
    })
}

async fn list_models(State(state): State<ProxyAppState>, headers: HeaderMap) -> Response {
    if !authorize_request("list_models", &headers, &state.fake_api_key) {
        return unauthorized_response();
    }

    (StatusCode::OK, Json(list_models_payload(&state.config))).into_response()
}

async fn get_model(
    State(state): State<ProxyAppState>,
    Path(model_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorize_request("get_model", &headers, &state.fake_api_key) {
        return unauthorized_response();
    }

    if !state.config.models.supports(&model_id) {
        tracing::warn!(route_kind = "get_model", %model_id, "client requested unavailable model");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("model `{model_id}` is not available through codex-proxy"),
                    "type": "invalid_request_error"
                }
            })),
        )
            .into_response();
    }

    (StatusCode::OK, Json(model_payload(&model_id))).into_response()
}

async fn forward_v1_responses(
    State(state): State<ProxyAppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorize_request("responses", &headers, &state.fake_api_key) {
        return unauthorized_response();
    }

    forward_responses(&state.forwarding_state(), &headers, body).await
}

async fn forward_v1_responses_compact(
    State(state): State<ProxyAppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorize_request("compact", &headers, &state.fake_api_key) {
        return unauthorized_response();
    }

    forward_responses_compact(&state.forwarding_state(), &headers, body).await
}

async fn forward_v1_chat_completions(
    State(state): State<ProxyAppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorize_request("chat_completions", &headers, &state.fake_api_key) {
        return unauthorized_response();
    }

    forward_chat_completions(&state.forwarding_state(), &headers, body).await
}

/// Return `true` if the request carries the correct fake API key.
fn authorize(headers: &HeaderMap, expected_key: &str) -> bool {
    matches!(extract_api_key(headers), Some(candidate) if candidate == expected_key)
}

fn authorize_request(route_kind: &'static str, headers: &HeaderMap, expected_key: &str) -> bool {
    let authorized = authorize(headers, expected_key);
    if !authorized {
        tracing::warn!(
            route_kind,
            auth_source = auth_source(headers),
            "rejected request with invalid or missing fake API key"
        );
    }
    authorized
}

fn auth_source(headers: &HeaderMap) -> &'static str {
    if headers.contains_key(AUTHORIZATION) {
        "authorization"
    } else if headers.contains_key("x-api-key") {
        "x-api-key"
    } else {
        "missing"
    }
}

/// Build a 401 JSON response used when authentication fails.
fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": "Invalid or missing fake API key for codex-proxy.",
                "type": "invalid_api_key"
            }
        })),
    )
        .into_response()
}

fn list_models_payload(config: &Config) -> Value {
    let data = config
        .models
        .advertised
        .iter()
        .map(|model| model_payload(model))
        .collect::<Vec<_>>();

    json!({
        "object": "list",
        "data": data
    })
}

fn model_payload(model_id: &str) -> Value {
    json!({
        "id": model_id,
        "object": "model",
        "created": 0,
        "owned_by": "openai",
        "root": model_id,
        "permission": []
    })
}

/// Extract the API key from the request headers.
///
/// Supports two formats:
/// - `Authorization: Bearer <key>` (case-insensitive `bearer` prefix)
/// - `x-api-key: <key>`
///
/// `Authorization` is checked first.
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(rest) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
    {
        return Some(rest.trim().to_owned());
    }

    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header::AUTHORIZATION};

    use super::*;

    #[test]
    fn extract_api_key_supports_bearer_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer local-key"));

        let key = extract_api_key(&headers);

        assert_eq!(key.as_deref(), Some("local-key"));
    }

    #[test]
    fn extract_api_key_supports_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("local-key"));

        let key = extract_api_key(&headers);

        assert_eq!(key.as_deref(), Some("local-key"));
    }

    #[test]
    fn health_payload_distinguishes_auth_and_forwarding_readiness() {
        let tokens = TokenStore {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: i64::MAX,
            token_type: "Bearer".to_owned(),
            scope: "openid".to_owned(),
            account_id: Some("account-123".to_owned()),
            obtained_at: 1,
        };

        let payload = health_payload(Some(&tokens), true, true);

        assert_eq!(payload["daemon_ready"], true);
        assert_eq!(payload["auth_ready"], true);
        assert_eq!(payload["forwarding_ready"], true);
        assert_eq!(
            payload["chat_completions_compatibility"]["implemented"],
            true
        );
        assert_eq!(
            payload["chat_completions_compatibility"]["streaming_supported"],
            false
        );
    }

    #[test]
    fn list_models_payload_includes_openai_compatible_model_objects() {
        let config = Config::default();
        let payload = list_models_payload(&config);
        let data = payload["data"].as_array().expect("models list");

        assert_eq!(payload["object"], "list");
        assert_eq!(data.len(), config.models.advertised.len());
        assert_eq!(data[0]["object"], "model");
        assert_eq!(data[0]["root"], data[0]["id"]);
        assert!(data[0]["permission"].as_array().is_some());
    }

    #[test]
    fn model_payload_uses_requested_model_id() {
        let payload = model_payload("gpt-5.3-codex");

        assert_eq!(payload["id"], "gpt-5.3-codex");
        assert_eq!(payload["root"], "gpt-5.3-codex");
        assert_eq!(payload["owned_by"], "openai");
    }
}

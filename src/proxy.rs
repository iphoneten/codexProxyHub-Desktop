use crate::config::{AppConfig, ProviderConfig};
use anyhow::{anyhow, Result};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    client: Client,
    counters: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
}

#[derive(Debug)]
struct ProxyError {
    status: StatusCode,
    message: String,
}

impl ProxyError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": "proxy_error",
                    "code": self.status.as_u16()
                }
            })),
        )
            .into_response()
    }
}

pub async fn run_server(config: AppConfig, shutdown: oneshot::Receiver<()>) -> Result<()> {
    let bind_host = if config.server.host == "0.0.0.0" {
        "0.0.0.0"
    } else {
        config.server.host.as_str()
    };
    let addr: SocketAddr = format!("{}:{}", bind_host, config.server.port).parse()?;
    let state = AppState {
        config: Arc::new(config),
        client: Client::builder()
            .pool_max_idle_per_host(20)
            .danger_accept_invalid_certs(false)
            .build()?,
        counters: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/v1", get(index))
        .route("/v1/", get(index))
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/models/:model", get(get_model))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/responses", post(responses))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await?;
    Ok(())
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "name": "recodexProxyHub",
        "ok": true,
        "base_url": format!("http://{}:{}/v1", display_host(&state.config.server.host), state.config.server.port),
        "endpoints": [
            "/health",
            "/v1/models",
            "/v1/chat/completions",
            "/v1/completions",
            "/v1/embeddings",
            "/v1/responses"
        ]
    }))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "providers": state.config.providers.iter().filter(|p| p.enabled).count(),
        "models": collect_models(&state.config).len(),
    }))
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ProxyError> {
    authorize(&state.config, &headers)?;
    let data: Vec<Value> = collect_models(&state.config)
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "recodex-proxy-hub"}))
        .collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn get_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(model): Path<String>,
) -> Result<impl IntoResponse, ProxyError> {
    authorize(&state.config, &headers)?;
    if collect_models(&state.config).contains(&model) {
        Ok(Json(
            json!({"id": model, "object": "model", "created": 0, "owned_by": "recodex-proxy-hub"}),
        ))
    } else {
        Err(ProxyError::new(StatusCode::NOT_FOUND, "模型不存在"))
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    authorize(&state.config, &headers)?;
    forward_openai(state, headers, body, "/chat/completions", "chat").await
}

async fn completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    authorize(&state.config, &headers)?;
    forward_openai(state, headers, body, "/completions", "completion").await
}

async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    authorize(&state.config, &headers)?;
    forward_openai(state, headers, body, "/embeddings", "embedding").await
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ProxyError> {
    authorize(&state.config, &headers)?;
    let model = body_model(&body)?;
    let providers = provider_attempts(&state, &model);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);

        let force_chat = provider.responses_mode == "chat" || provider.provider_type == "anthropic";
        if force_chat {
            match forward_responses_as_chat(&state, &provider, &headers, upstream_body, stream)
                .await
            {
                Ok(resp) => {
                    log_usage(
                        &state.config,
                        "responses",
                        &provider.name,
                        &request_model,
                        started,
                        None,
                    );
                    return Ok(resp);
                }
                Err(err) => last_error = Some(err.message),
            }
            continue;
        }

        match send_to_provider(
            &state.client,
            &provider,
            "/responses",
            &headers,
            upstream_body,
            stream,
        )
        .await
        {
            Ok(resp) => {
                log_usage(
                    &state.config,
                    "responses",
                    &provider.name,
                    &request_model,
                    started,
                    None,
                );
                return Ok(resp);
            }
            Err(err)
                if provider.responses_mode == "auto"
                    && (err.status == StatusCode::NOT_FOUND
                        || err.status == StatusCode::METHOD_NOT_ALLOWED) =>
            {
                let chat_body = with_model(body.clone(), &request_model);
                match forward_responses_as_chat(&state, &provider, &headers, chat_body, stream)
                    .await
                {
                    Ok(resp) => {
                        log_usage(
                            &state.config,
                            "responses",
                            &provider.name,
                            &request_model,
                            started,
                            None,
                        );
                        return Ok(resp);
                    }
                    Err(chat_err) => last_error = Some(chat_err.message),
                }
            }
            Err(err) => {
                if !retryable_status(err.status) {
                    return Err(err);
                }
                last_error = Some(err.message);
            }
        }
    }

    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        last_error.unwrap_or_else(|| format!("模型 '{}' 没有可用渠道", model)),
    ))
}

async fn forward_openai(
    state: AppState,
    headers: HeaderMap,
    body: Value,
    path: &'static str,
    api: &'static str,
) -> Result<Response, ProxyError> {
    let model = body_model(&body)?;
    let providers = provider_attempts(&state, &model);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut last_error = None;

    for (provider, request_model) in providers {
        let started = Instant::now();
        let mut upstream_body = with_model(body.clone(), &request_model);
        apply_model_mapping(&provider, &mut upstream_body);
        if path == "/chat/completions" {
            apply_system_prompt_override(&provider, &mut upstream_body);
        }
        match send_to_provider(
            &state.client,
            &provider,
            path,
            &headers,
            upstream_body,
            stream,
        )
        .await
        {
            Ok(resp) => {
                log_usage(
                    &state.config,
                    api,
                    &provider.name,
                    &request_model,
                    started,
                    None,
                );
                return Ok(resp);
            }
            Err(err) => {
                if !retryable_status(err.status) {
                    return Err(err);
                }
                last_error = Some(err.message);
            }
        }
    }

    Err(ProxyError::new(
        StatusCode::BAD_GATEWAY,
        last_error.unwrap_or_else(|| format!("模型 '{}' 没有可用渠道", model)),
    ))
}

async fn forward_responses_as_chat(
    state: &AppState,
    provider: &ProviderConfig,
    headers: &HeaderMap,
    body: Value,
    stream: bool,
) -> Result<Response, ProxyError> {
    let chat_body = responses_to_chat_body(&body)?;
    if stream {
        return send_to_provider(
            &state.client,
            provider,
            "/chat/completions",
            headers,
            chat_body,
            true,
        )
        .await;
    }

    let upstream = send_json_to_provider(
        &state.client,
        provider,
        "/chat/completions",
        headers,
        chat_body,
    )
    .await?;
    let wrapped = chat_to_response(
        upstream,
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    Ok((StatusCode::OK, Json(wrapped)).into_response())
}

async fn send_to_provider(
    client: &Client,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
) -> Result<Response, ProxyError> {
    if stream {
        let url = upstream_url(provider, path);
        let req = client
            .post(url)
            .timeout(Duration::from_secs(provider.timeout.max(1)))
            .headers(upstream_headers(provider, request_headers, true))
            .json(&body);
        for attempt in 0..=provider.max_retries {
            match req.try_clone().unwrap().send().await {
                Ok(resp) if resp.status().is_success() => {
                    let status =
                        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
                    let response = Response::builder()
                        .status(status)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from_stream(resp.bytes_stream()))
                        .map_err(|e| ProxyError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
                    return Ok(response);
                }
                Ok(resp) => {
                    let status = StatusCode::from_u16(resp.status().as_u16())
                        .unwrap_or(StatusCode::BAD_GATEWAY);
                    let text = resp.text().await.unwrap_or_default();
                    if attempt < provider.max_retries && retryable_status(status) {
                        tokio::time::sleep(retry_delay(attempt)).await;
                        continue;
                    }
                    return Err(ProxyError::new(status, truncate(&text)));
                }
                Err(err) if attempt < provider.max_retries => {
                    tokio::time::sleep(retry_delay(attempt)).await;
                    if err.is_timeout() || err.is_connect() || err.is_request() {
                        continue;
                    }
                    return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()));
                }
                Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string())),
            }
        }
        unreachable!();
    }

    let value = send_json_to_provider(client, provider, path, request_headers, body).await?;
    Ok((StatusCode::OK, Json(value)).into_response())
}

async fn send_json_to_provider(
    client: &Client,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
) -> Result<Value, ProxyError> {
    let url = upstream_url(provider, path);
    for attempt in 0..=provider.max_retries {
        let result = client
            .post(url.clone())
            .timeout(Duration::from_secs(provider.timeout.max(1)))
            .headers(upstream_headers(provider, request_headers, false))
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                return resp.json::<Value>().await.map_err(|e| {
                    ProxyError::new(StatusCode::BAD_GATEWAY, format!("上游 JSON 解析失败: {e}"))
                });
            }
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let text = resp.text().await.unwrap_or_default();
                if attempt < provider.max_retries && retryable_status(status) {
                    tokio::time::sleep(retry_delay(attempt)).await;
                    continue;
                }
                return Err(ProxyError::new(status, truncate(&text)));
            }
            Err(err) if attempt < provider.max_retries => {
                tokio::time::sleep(retry_delay(attempt)).await;
                if err.is_timeout() || err.is_connect() || err.is_request() {
                    continue;
                }
                return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()));
            }
            Err(err) => return Err(ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string())),
        }
    }
    Err(ProxyError::new(StatusCode::BAD_GATEWAY, "上游请求失败"))
}

fn authorize(config: &AppConfig, headers: &HeaderMap) -> Result<(), ProxyError> {
    if !config.auth.enabled {
        return Ok(());
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    if config
        .auth
        .api_keys
        .iter()
        .any(|k| k.enabled && k.key == token)
    {
        Ok(())
    } else {
        Err(ProxyError::new(
            StatusCode::UNAUTHORIZED,
            "无效或缺失 API Key",
        ))
    }
}

fn provider_attempts(state: &AppState, model: &str) -> Vec<(ProviderConfig, String)> {
    let mut attempts = vec![model.to_string()];
    if let Some(fallbacks) = state.config.routing.model_fallbacks.get(model) {
        for item in fallbacks {
            if !attempts.contains(item) {
                attempts.push(item.clone());
            }
        }
    }

    let mut out = Vec::new();
    for request_model in attempts {
        let mut providers: Vec<ProviderConfig> = state
            .config
            .providers
            .iter()
            .filter(|p| p.enabled && provider_supports_model(p, &request_model))
            .cloned()
            .collect();
        providers.sort_by_key(|p| (p.priority, p.name.clone()));
        let mut by_priority: HashMap<i32, Vec<ProviderConfig>> = HashMap::new();
        for provider in providers {
            by_priority
                .entry(provider.priority)
                .or_default()
                .push(provider);
        }
        let mut priorities: Vec<_> = by_priority.keys().copied().collect();
        priorities.sort();
        for priority in priorities {
            let weighted = weighted_order(
                state,
                &request_model,
                by_priority.remove(&priority).unwrap_or_default(),
            );
            out.extend(weighted.into_iter().map(|p| (p, request_model.clone())));
        }
    }
    out
}

fn weighted_order(
    state: &AppState,
    model: &str,
    providers: Vec<ProviderConfig>,
) -> Vec<ProviderConfig> {
    let mut expanded = Vec::new();
    for provider in providers {
        for _ in 0..provider.weight.max(1) {
            expanded.push(provider.clone());
        }
    }
    if expanded.is_empty() {
        return expanded;
    }
    let counter = {
        let mut counters = state.counters.lock();
        counters
            .entry(model.to_string())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone()
    };
    let start = counter.fetch_add(1, Ordering::Relaxed) % expanded.len();
    expanded.rotate_left(start);
    expanded
}

fn provider_supports_model(provider: &ProviderConfig, model: &str) -> bool {
    let normalized = normalize_model(model);
    provider
        .models
        .iter()
        .any(|m| normalize_model(m) == normalized)
        || provider
            .model_mapping
            .keys()
            .any(|m| normalize_model(m) == normalized)
}

fn collect_models(config: &AppConfig) -> Vec<String> {
    let mut models = Vec::new();
    for provider in config.providers.iter().filter(|p| p.enabled) {
        for model in &provider.models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        for model in provider.model_mapping.keys() {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
    }
    models.sort();
    models
}

fn body_model(body: &Value) -> Result<String, ProxyError> {
    body.get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ProxyError::new(StatusCode::BAD_REQUEST, "请求体缺少 model"))
}

fn with_model(mut body: Value, model: &str) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
    }
    body
}

fn apply_model_mapping(provider: &ProviderConfig, body: &mut Value) {
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return;
    };
    let normalized = normalize_model(model);
    let mapped = provider
        .model_mapping
        .get(model)
        .cloned()
        .or_else(|| {
            provider
                .model_mapping
                .iter()
                .find(|(k, _)| normalize_model(k) == normalized)
                .map(|(_, v)| v.clone())
        })
        .or_else(|| {
            provider
                .models
                .iter()
                .find(|m| normalize_model(m) == normalized)
                .cloned()
        });
    if let (Some(obj), Some(mapped)) = (body.as_object_mut(), mapped) {
        obj.insert("model".to_string(), Value::String(mapped));
    }
}

fn apply_system_prompt_override(provider: &ProviderConfig, body: &mut Value) {
    let Some(prompt) = provider
        .system_prompt_override
        .as_ref()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(messages) = obj.get("messages").and_then(Value::as_array) else {
        return;
    };
    let mut next = vec![json!({"role": "system", "content": prompt})];
    next.extend(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
            .cloned(),
    );
    obj.insert("messages".to_string(), Value::Array(next));
}

fn responses_to_chat_body(body: &Value) -> Result<Value, ProxyError> {
    let mut obj = Map::new();
    obj.insert(
        "model".to_string(),
        body.get("model")
            .cloned()
            .unwrap_or(Value::String(String::new())),
    );
    obj.insert(
        "messages".to_string(),
        Value::Array(responses_input_to_messages(body.get("input"))),
    );
    for key in [
        "temperature",
        "top_p",
        "max_tokens",
        "max_output_tokens",
        "stream",
        "tools",
        "tool_choice",
    ] {
        if let Some(value) = body.get(key) {
            let target = if key == "max_output_tokens" {
                "max_tokens"
            } else {
                key
            };
            obj.insert(target.to_string(), value.clone());
        }
    }
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if let Some(Value::Array(messages)) = obj.get_mut("messages") {
            messages.insert(0, json!({"role": "system", "content": instructions}));
        }
    }
    Ok(Value::Object(obj))
}

fn responses_input_to_messages(input: Option<&Value>) -> Vec<Value> {
    match input {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let content = match item.get("content") {
                    Some(Value::String(text)) => Value::String(text.clone()),
                    Some(Value::Array(parts)) => {
                        let text = parts
                            .iter()
                            .filter_map(|p| {
                                p.get("text")
                                    .or_else(|| p.get("input_text"))
                                    .and_then(Value::as_str)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Value::String(text)
                    }
                    _ => Value::String(String::new()),
                };
                Some(json!({"role": role, "content": content}))
            })
            .collect(),
        _ => vec![json!({"role": "user", "content": ""})],
    }
}

fn chat_to_response(chat: Value, request_model: &str) -> Value {
    let text = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let model = chat
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(request_model);
    json!({
        "id": format!("resp-{}", Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": [{
            "id": format!("msg-{}", Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }],
        "output_text": text,
        "usage": chat.get("usage").cloned().unwrap_or(Value::Null)
    })
}

fn upstream_url(provider: &ProviderConfig, path: &str) -> String {
    format!("{}{}", provider.base_url.trim_end_matches('/'), path)
}

fn upstream_headers(
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    stream: bool,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if provider.provider_type == "anthropic" {
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&provider.api_key)
                .unwrap_or_else(|_| HeaderValue::from_static("")),
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    } else if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", provider.api_key)) {
        headers.insert(header::AUTHORIZATION, value);
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    if stream {
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }

    for (name, value) in &provider.extra_headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization" | "content-type" | "accept" | "host" | "content-length" | "x-api-key"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    for passthrough in [
        "user-agent",
        "openai-beta",
        "openai-version",
        "openai-organization",
        "openai-project",
    ] {
        if let Some(value) = request_headers.get(passthrough) {
            if let Ok(name) = http::header::HeaderName::from_bytes(passthrough.as_bytes()) {
                headers.insert(name, value.clone());
            }
        }
    }
    headers
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || status == StatusCode::BAD_GATEWAY
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(
        (200_u64)
            .saturating_mul(2_u64.saturating_pow(attempt as u32))
            .min(2_000),
    )
}

fn normalize_model(model: &str) -> &str {
    model.strip_prefix("models/").unwrap_or(model)
}

fn truncate(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > 500 {
        format!(
            "{}...(truncated)",
            trimmed.chars().take(500).collect::<String>()
        )
    } else {
        trimmed.to_string()
    }
}

fn display_host(host: &str) -> &str {
    if host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        host
    }
}

fn log_usage(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    token_source: Option<&str>,
) {
    let record = json!({
        "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
        "api": api,
        "channel": provider,
        "request_model": model,
        "latency_ms": started.elapsed().as_millis(),
        "token_source": token_source.unwrap_or("upstream_or_unknown"),
    });
    let path = PathBuf::from(&config.usage_log.path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", record);
    }
}

pub fn validate_config(config: &AppConfig) -> Result<()> {
    if config.providers.iter().filter(|p| p.enabled).count() == 0 {
        return Err(anyhow!("没有启用的 provider"));
    }
    Ok(())
}

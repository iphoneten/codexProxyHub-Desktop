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
use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    client: Client,
    counters: Arc<Mutex<HashMap<String, Arc<AtomicUsize>>>>,
}

#[derive(Clone, Copy, Default)]
struct TokenUsage {
    input: i64,
    output: i64,
}

struct ProviderResult {
    response: Response,
    upstream_model: String,
    usage: TokenUsage,
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

        let force_chat = provider.responses_mode == "chat"
            || provider.provider_type == "anthropic"
            || matches!(provider.capabilities.get("supports_responses"), Some(false));
        if force_chat {
            match forward_responses_as_chat(&state, &provider, &headers, upstream_body, stream)
                .await
            {
                Ok(result) => {
                    log_success(
                        &state.config,
                        "responses",
                        &provider.name,
                        &model,
                        &result.upstream_model,
                        result.usage,
                        started,
                        stream,
                    );
                    return Ok(result.response);
                }
                Err(err) => {
                    let message = err.message;
                    log_error(
                        &state.config,
                        "responses",
                        &provider.name,
                        &request_model,
                        started,
                        &message,
                    );
                    last_error = Some(message);
                }
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
            Ok(result) => {
                log_success(
                    &state.config,
                    "responses",
                    &provider.name,
                    &model,
                    &result.upstream_model,
                    result.usage,
                    started,
                    stream,
                );
                return Ok(result.response);
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
                    Ok(result) => {
                        log_success(
                            &state.config,
                            "responses",
                            &provider.name,
                            &model,
                            &result.upstream_model,
                            result.usage,
                            started,
                            stream,
                        );
                        return Ok(result.response);
                    }
                    Err(chat_err) => {
                        let message = chat_err.message;
                        log_error(
                            &state.config,
                            "responses",
                            &provider.name,
                            &request_model,
                            started,
                            &message,
                        );
                        last_error = Some(message);
                    }
                }
            }
            Err(err) => {
                let retryable = retryable_status(err.status);
                let status = err.status;
                let message = err.message;
                log_error(
                    &state.config,
                    "responses",
                    &provider.name,
                    &request_model,
                    started,
                    &message,
                );
                if !retryable {
                    return Err(ProxyError::new(status, message));
                }
                if retryable {
                    last_error = Some(message);
                }
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
            Ok(result) => {
                log_success(
                    &state.config,
                    api,
                    &provider.name,
                    &model,
                    &result.upstream_model,
                    result.usage,
                    started,
                    stream,
                );
                return Ok(result.response);
            }
            Err(err) => {
                let retryable = retryable_status(err.status);
                let status = err.status;
                let message = err.message;
                log_error(
                    &state.config,
                    api,
                    &provider.name,
                    &request_model,
                    started,
                    &message,
                );
                if !retryable {
                    return Err(ProxyError::new(status, message));
                }
                last_error = Some(message);
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
) -> Result<ProviderResult, ProxyError> {
    let chat_body = responses_to_chat_body(&body)?;
    if stream {
        let request_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return send_chat_stream_as_responses(
            &state.client,
            provider,
            headers,
            chat_body,
            request_model,
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
    let upstream_model = response_model(&wrapped).unwrap_or_else(|| {
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    let usage = extract_token_usage(&wrapped);
    Ok(ProviderResult {
        response: (StatusCode::OK, Json(wrapped)).into_response(),
        upstream_model,
        usage,
    })
}

async fn send_chat_stream_as_responses(
    client: &Client,
    provider: &ProviderConfig,
    request_headers: &HeaderMap,
    body: Value,
    request_model: String,
) -> Result<ProviderResult, ProxyError> {
    let url = upstream_url(provider, "/chat/completions");
    let req = client
        .post(url)
        .timeout(Duration::from_secs(provider.timeout.max(1)))
        .headers(upstream_headers(provider, request_headers, true))
        .json(&body);

    for attempt in 0..=provider.max_retries {
        match req.try_clone().unwrap().send().await {
            Ok(resp) if resp.status().is_success() => {
                return chat_stream_to_responses(resp, request_model);
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

fn chat_stream_to_responses(
    resp: reqwest::Response,
    request_model: String,
) -> Result<ProviderResult, ProxyError> {
    let model = if request_model.is_empty() {
        "unknown".to_string()
    } else {
        request_model
    };
    let result_model = model.clone();
    let stream_model = model.clone();
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    tokio::spawn(async move {
        let response_id = format!("resp-{}", Uuid::new_v4().simple());
        let item_id = format!("msg-{}", Uuid::new_v4().simple());
        let created_at = chrono::Utc::now().timestamp();
        let model = stream_model;
        let _ = send_response_sse(
            &tx,
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": created_at,
                    "status": "in_progress",
                    "model": model,
                    "output": []
                }
            }),
        )
        .await;
        let _ = send_response_sse(
            &tx,
            "response.in_progress",
            json!({
                "type": "response.in_progress",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": created_at,
                    "status": "in_progress",
                    "model": model,
                    "output": []
                }
            }),
        )
        .await;
        let _ = send_response_sse(
            &tx,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            }),
        )
        .await;
        let _ = send_response_sse(
            &tx,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": ""}
            }),
        )
        .await;

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut full_text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some((event, consumed)) = next_sse_event(&buffer) {
                        buffer.drain(..consumed);
                        let Some(data) = sse_data(&event) else {
                            continue;
                        };
                        if data.trim() == "[DONE]" {
                            send_response_stream_done(
                                &tx,
                                &response_id,
                                &item_id,
                                &model,
                                created_at,
                                &full_text,
                            )
                            .await;
                            return;
                        }
                        if let Some(delta) = chat_stream_delta(&data) {
                            full_text.push_str(&delta);
                            let _ = send_response_sse(
                                &tx,
                                "response.output_text.delta",
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": item_id,
                                    "output_index": 0,
                                    "content_index": 0,
                                    "delta": delta
                                }),
                            )
                            .await;
                        }
                    }
                }
                Err(err) => {
                    let _ = send_response_sse(
                        &tx,
                        "response.failed",
                        json!({
                            "type": "response.failed",
                            "response": {
                                "id": response_id,
                                "status": "failed",
                                "model": model,
                                "error": {"message": err.to_string()}
                            }
                        }),
                    )
                    .await;
                    return;
                }
            }
        }

        send_response_stream_done(&tx, &response_id, &item_id, &model, created_at, &full_text)
            .await;
    });

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .map_err(|err| ProxyError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;
    Ok(ProviderResult {
        response,
        upstream_model: result_model,
        usage: TokenUsage::default(),
    })
}

async fn send_response_stream_done(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    response_id: &str,
    item_id: &str,
    model: &str,
    created_at: i64,
    full_text: &str,
) {
    let _ = send_response_sse(
        tx,
        "response.output_text.done",
        json!({
            "type": "response.output_text.done",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "text": full_text
        }),
    )
    .await;
    let _ = send_response_sse(
        tx,
        "response.content_part.done",
        json!({
            "type": "response.content_part.done",
            "item_id": item_id,
            "output_index": 0,
            "content_index": 0,
            "part": {"type": "output_text", "text": full_text}
        }),
    )
    .await;
    let _ = send_response_sse(
        tx,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": full_text}]
            }
        }),
    )
    .await;
    let _ = send_response_sse(
        tx,
        "response.completed",
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "created_at": created_at,
                "status": "completed",
                "model": model,
                "output": [{
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": full_text}]
                }],
                "output_text": full_text
            }
        }),
    )
    .await;
    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
}

async fn send_response_sse(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    event: &str,
    data: Value,
) -> Result<(), mpsc::error::SendError<Result<Bytes, io::Error>>> {
    tx.send(Ok(Bytes::from(format!("event: {event}\ndata: {data}\n\n"))))
        .await
}

fn next_sse_event(buffer: &str) -> Option<(String, usize)> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    let (idx, sep_len) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return None,
    };
    Some((buffer[..idx].to_string(), idx + sep_len))
}

fn sse_data(event: &str) -> Option<String> {
    let data = event
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        None
    } else {
        Some(data)
    }
}

fn chat_stream_delta(data: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(data).ok()?;
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .or_else(|| choice.get("text"))
        })
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

async fn send_to_provider(
    client: &Client,
    provider: &ProviderConfig,
    path: &str,
    request_headers: &HeaderMap,
    body: Value,
    stream: bool,
) -> Result<ProviderResult, ProxyError> {
    if stream {
        let upstream_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
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
                    return Ok(ProviderResult {
                        response,
                        upstream_model,
                        usage: TokenUsage::default(),
                    });
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

    let request_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let value = send_json_to_provider(client, provider, path, request_headers, body).await?;
    let upstream_model = response_model(&value).unwrap_or(request_model);
    let usage = extract_token_usage(&value);
    Ok(ProviderResult {
        response: (StatusCode::OK, Json(value)).into_response(),
        upstream_model,
        usage,
    })
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
            .map(|item| {
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
                json!({"role": role, "content": content})
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

fn response_model(value: &Value) -> Option<String> {
    value
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_token_usage(value: &Value) -> TokenUsage {
    let Some(usage) = value.get("usage") else {
        return TokenUsage::default();
    };
    TokenUsage {
        input: usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        output: usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    }
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

struct UsageLogEvent<'a> {
    api: &'a str,
    provider: &'a str,
    model: &'a str,
    upstream_model: &'a str,
    status: &'a str,
    error: Option<&'a str>,
    usage: TokenUsage,
    token_source: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn log_success(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    usage: TokenUsage,
    started: Instant,
    stream: bool,
) {
    log_usage(
        config,
        started,
        UsageLogEvent {
            api,
            provider,
            model,
            upstream_model,
            status: if stream { "stream_started" } else { "ok" },
            error: None,
            usage,
            token_source: None,
        },
    );
}

fn log_error(
    config: &AppConfig,
    api: &str,
    provider: &str,
    model: &str,
    started: Instant,
    error: &str,
) {
    log_usage(
        config,
        started,
        UsageLogEvent {
            api,
            provider,
            model,
            upstream_model: "",
            status: "error",
            error: Some(error),
            usage: TokenUsage::default(),
            token_source: None,
        },
    );
}

fn log_usage(config: &AppConfig, started: Instant, event: UsageLogEvent<'_>) {
    let ts = chrono::Local::now()
        .format("%Y年%m月%d日 %H:%M:%S")
        .to_string();
    let latency_ms = started.elapsed().as_millis() as i64;
    let error = event.error.unwrap_or("");
    let token_source = event.token_source.unwrap_or("upstream_or_unknown");

    if config.usage_log.backend.eq_ignore_ascii_case("sqlite")
        && log_usage_sqlite(
            &config.usage_log.sqlite_path,
            &ts,
            event.api,
            event.status,
            event.provider,
            event.model,
            event.upstream_model,
            latency_ms,
            error,
            event.usage.input,
            event.usage.output,
            token_source,
        )
        .is_ok()
    {
        return;
    }

    let record = json!({
        "ts": ts,
        "api": event.api,
        "status": event.status,
        "channel": event.provider,
        "request_model": event.model,
        "upstream_model": event.upstream_model,
        "latency_ms": latency_ms,
        "input_tokens": event.usage.input,
        "output_tokens": event.usage.output,
        "error": error,
        "token_source": token_source,
    });
    let path = PathBuf::from(&config.usage_log.path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", record);
    }
}

#[allow(clippy::too_many_arguments)]
fn log_usage_sqlite(
    path: &str,
    ts: &str,
    api: &str,
    status: &str,
    provider: &str,
    model: &str,
    upstream_model: &str,
    latency_ms: i64,
    error: &str,
    input_tokens: i64,
    output_tokens: i64,
    token_source: &str,
) -> rusqlite::Result<()> {
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    ensure_usage_log_schema(&conn)?;
    conn.execute(
        r#"
        INSERT INTO usage_logs
            (
                ts, api, status, channel, request_model, upstream_model,
                latency_ms, input_tokens, output_tokens, error, token_source
            )
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        "#,
        params![
            ts,
            api,
            status,
            provider,
            model,
            upstream_model,
            latency_ms,
            input_tokens,
            output_tokens,
            error,
            token_source
        ],
    )?;
    Ok(())
}

pub fn ensure_usage_log_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS usage_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            api TEXT NOT NULL,
            status TEXT NOT NULL,
            channel TEXT NOT NULL,
            request_model TEXT NOT NULL,
            upstream_model TEXT NOT NULL DEFAULT '',
            latency_ms INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            error TEXT NOT NULL DEFAULT '',
            token_source TEXT NOT NULL DEFAULT 'upstream_or_unknown'
        );
        CREATE INDEX IF NOT EXISTS idx_usage_logs_ts ON usage_logs(ts);
        CREATE INDEX IF NOT EXISTS idx_usage_logs_status ON usage_logs(status);
        CREATE INDEX IF NOT EXISTS idx_usage_logs_channel ON usage_logs(channel);
        "#,
    )?;
    ensure_column(conn, "upstream_model", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "input_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "output_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(())
}

fn ensure_column(conn: &Connection, name: &str, definition: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(usage_logs)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for column in columns {
        if column? == name {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE usage_logs ADD COLUMN {name} {definition}"),
        [],
    )?;
    Ok(())
}

pub fn validate_config(config: &AppConfig) -> Result<()> {
    if config.providers.iter().filter(|p| p.enabled).count() == 0 {
        return Err(anyhow!("没有启用的 provider"));
    }
    Ok(())
}
